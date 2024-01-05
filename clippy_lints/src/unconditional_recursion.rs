use clippy_utils::diagnostics::span_lint_and_then;
use clippy_utils::{expr_or_init, get_trait_def_id, path_def_id};
use rustc_ast::BinOpKind;
use rustc_data_structures::fx::FxHashMap;
use rustc_hir as hir;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_hir::intravisit::{walk_body, walk_expr, FnKind, Visitor};
use rustc_hir::{Body, Expr, ExprKind, FnDecl, HirId, Item, ItemKind, Node, QPath, TyKind};
use rustc_hir_analysis::hir_ty_to_ty;
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::hir::map::Map;
use rustc_middle::hir::nested_filter;
use rustc_middle::ty::{self, AssocKind, Ty, TyCtxt};
use rustc_session::impl_lint_pass;
use rustc_span::symbol::{kw, Ident};
use rustc_span::{sym, Span};
use rustc_trait_selection::traits::error_reporting::suggestions::ReturnsVisitor;

declare_clippy_lint! {
    /// ### What it does
    /// Checks that there isn't an infinite recursion in `PartialEq` trait
    /// implementation.
    ///
    /// ### Why is this bad?
    /// This is a hard to find infinite recursion which will crashing any code
    /// using it.
    ///
    /// ### Example
    /// ```no_run
    /// enum Foo {
    ///     A,
    ///     B,
    /// }
    ///
    /// impl PartialEq for Foo {
    ///     fn eq(&self, other: &Self) -> bool {
    ///         self == other // bad!
    ///     }
    /// }
    /// ```
    /// Use instead:
    ///
    /// In such cases, either use `#[derive(PartialEq)]` or don't implement it.
    #[clippy::version = "1.76.0"]
    pub UNCONDITIONAL_RECURSION,
    suspicious,
    "detect unconditional recursion in some traits implementation"
}

#[derive(Default)]
pub struct UnconditionalRecursion {
    /// The key is the `DefId` of the type implementing the `Default` trait and the value is the
    /// `DefId` of the return call.
    default_impl_for_type: FxHashMap<DefId, DefId>,
}

impl_lint_pass!(UnconditionalRecursion => [UNCONDITIONAL_RECURSION]);

fn span_error(cx: &LateContext<'_>, method_span: Span, expr: &Expr<'_>) {
    span_lint_and_then(
        cx,
        UNCONDITIONAL_RECURSION,
        method_span,
        "function cannot return without recursing",
        |diag| {
            diag.span_note(expr.span, "recursive call site");
        },
    );
}

fn get_ty_def_id(ty: Ty<'_>) -> Option<DefId> {
    match ty.peel_refs().kind() {
        ty::Adt(adt, _) => Some(adt.did()),
        ty::Foreign(def_id) => Some(*def_id),
        _ => None,
    }
}

fn get_hir_ty_def_id(tcx: TyCtxt<'_>, hir_ty: rustc_hir::Ty<'_>) -> Option<DefId> {
    let TyKind::Path(qpath) = hir_ty.kind else { return None };
    match qpath {
        QPath::Resolved(_, path) => path.res.opt_def_id(),
        QPath::TypeRelative(_, _) => {
            let ty = hir_ty_to_ty(tcx, &hir_ty);

            match ty.kind() {
                ty::Alias(ty::Projection, proj) => {
                    Res::<HirId>::Def(DefKind::Trait, proj.trait_ref(tcx).def_id).opt_def_id()
                },
                _ => None,
            }
        },
        QPath::LangItem(..) => None,
    }
}

fn get_return_calls_in_body<'tcx>(body: &'tcx Body<'tcx>) -> Vec<&'tcx Expr<'tcx>> {
    let mut visitor = ReturnsVisitor::default();

    visitor.visit_body(body);
    visitor.returns
}

fn has_conditional_return(body: &Body<'_>, expr: &Expr<'_>) -> bool {
    match get_return_calls_in_body(body).as_slice() {
        [] => false,
        [return_expr] => return_expr.hir_id != expr.hir_id,
        _ => true,
    }
}

fn get_impl_trait_def_id(cx: &LateContext<'_>, method_def_id: LocalDefId) -> Option<DefId> {
    let hir_id = cx.tcx.local_def_id_to_hir_id(method_def_id);
    if let Some((
        _,
        Node::Item(Item {
            kind: ItemKind::Impl(impl_),
            owner_id,
            ..
        }),
    )) = cx.tcx.hir().parent_iter(hir_id).next()
        // We exclude `impl` blocks generated from rustc's proc macros.
        && !cx.tcx.has_attr(*owner_id, sym::automatically_derived)
        // It is a implementation of a trait.
        && let Some(trait_) = impl_.of_trait
    {
        trait_.trait_def_id()
    } else {
        None
    }
}

#[allow(clippy::unnecessary_def_path)]
fn check_partial_eq(cx: &LateContext<'_>, method_span: Span, method_def_id: LocalDefId, name: Ident, expr: &Expr<'_>) {
    let args = cx
        .tcx
        .instantiate_bound_regions_with_erased(cx.tcx.fn_sig(method_def_id).skip_binder())
        .inputs();
    // That has two arguments.
    if let [self_arg, other_arg] = args
        && let Some(self_arg) = get_ty_def_id(*self_arg)
        && let Some(other_arg) = get_ty_def_id(*other_arg)
        // The two arguments are of the same type.
        && self_arg == other_arg
        && let Some(trait_def_id) = get_impl_trait_def_id(cx, method_def_id)
        // The trait is `PartialEq`.
        && Some(trait_def_id) == get_trait_def_id(cx, &["core", "cmp", "PartialEq"])
    {
        let to_check_op = if name.name == sym::eq {
            BinOpKind::Eq
        } else {
            BinOpKind::Ne
        };
        let is_bad = match expr.kind {
            ExprKind::Binary(op, left, right) if op.node == to_check_op => {
                // Then we check if the left-hand element is of the same type as `self`.
                if let Some(left_ty) = cx.typeck_results().expr_ty_opt(left)
                    && let Some(left_id) = get_ty_def_id(left_ty)
                    && self_arg == left_id
                    && let Some(right_ty) = cx.typeck_results().expr_ty_opt(right)
                    && let Some(right_id) = get_ty_def_id(right_ty)
                    && other_arg == right_id
                {
                    true
                } else {
                    false
                }
            },
            ExprKind::MethodCall(segment, _receiver, &[_arg], _) if segment.ident.name == name.name => {
                if let Some(fn_id) = cx.typeck_results().type_dependent_def_id(expr.hir_id)
                    && let Some(trait_id) = cx.tcx.trait_of_item(fn_id)
                    && trait_id == trait_def_id
                {
                    true
                } else {
                    false
                }
            },
            _ => false,
        };
        if is_bad {
            span_error(cx, method_span, expr);
        }
    }
}

#[allow(clippy::unnecessary_def_path)]
fn check_to_string(cx: &LateContext<'_>, method_span: Span, method_def_id: LocalDefId, name: Ident, expr: &Expr<'_>) {
    let args = cx
        .tcx
        .instantiate_bound_regions_with_erased(cx.tcx.fn_sig(method_def_id).skip_binder())
        .inputs();
    // That has one argument.
    if let [_self_arg] = args
        && let hir_id = cx.tcx.local_def_id_to_hir_id(method_def_id)
        && let Some((
            _,
            Node::Item(Item {
                kind: ItemKind::Impl(impl_),
                owner_id,
                ..
            }),
        )) = cx.tcx.hir().parent_iter(hir_id).next()
        // We exclude `impl` blocks generated from rustc's proc macros.
        && !cx.tcx.has_attr(*owner_id, sym::automatically_derived)
        // It is a implementation of a trait.
        && let Some(trait_) = impl_.of_trait
        && let Some(trait_def_id) = trait_.trait_def_id()
        // The trait is `ToString`.
        && Some(trait_def_id) == get_trait_def_id(cx, &["alloc", "string", "ToString"])
    {
        let is_bad = match expr.kind {
            ExprKind::MethodCall(segment, _receiver, &[_arg], _) if segment.ident.name == name.name => {
                if let Some(fn_id) = cx.typeck_results().type_dependent_def_id(expr.hir_id)
                    && let Some(trait_id) = cx.tcx.trait_of_item(fn_id)
                    && trait_id == trait_def_id
                {
                    true
                } else {
                    false
                }
            },
            _ => false,
        };
        if is_bad {
            span_error(cx, method_span, expr);
        }
    }
}

fn is_default_method_on_current_ty(tcx: TyCtxt<'_>, qpath: QPath<'_>, implemented_ty_id: DefId) -> bool {
    match qpath {
        QPath::Resolved(_, path) => match path.segments {
            [first, .., last] => last.ident.name == kw::Default && first.res.opt_def_id() == Some(implemented_ty_id),
            _ => false,
        },
        QPath::TypeRelative(ty, segment) => {
            if segment.ident.name != kw::Default {
                return false;
            }
            if matches!(
                ty.kind,
                TyKind::Path(QPath::Resolved(
                    _,
                    hir::Path {
                        res: Res::SelfTyAlias { .. },
                        ..
                    },
                ))
            ) {
                return true;
            }
            get_hir_ty_def_id(tcx, *ty) == Some(implemented_ty_id)
        },
        QPath::LangItem(..) => false,
    }
}

struct CheckCalls<'a, 'tcx> {
    cx: &'a LateContext<'tcx>,
    map: Map<'tcx>,
    implemented_ty_id: DefId,
    found_default_call: bool,
    method_span: Span,
}

impl<'a, 'tcx> Visitor<'tcx> for CheckCalls<'a, 'tcx>
where
    'tcx: 'a,
{
    type NestedFilter = nested_filter::OnlyBodies;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.map
    }

    #[allow(clippy::unnecessary_def_path)]
    fn visit_expr(&mut self, expr: &'tcx Expr<'tcx>) {
        if self.found_default_call {
            return;
        }
        walk_expr(self, expr);

        if let ExprKind::Call(f, _) = expr.kind
            && let ExprKind::Path(qpath) = f.kind
            && is_default_method_on_current_ty(self.cx.tcx, qpath, self.implemented_ty_id)
            && let Some(method_def_id) = path_def_id(self.cx, f)
            && let Some(trait_def_id) = self.cx.tcx.trait_of_item(method_def_id)
            && Some(trait_def_id) == get_trait_def_id(self.cx, &["core", "default", "Default"])
        {
            self.found_default_call = true;
            span_error(self.cx, self.method_span, expr);
        }
    }
}

impl UnconditionalRecursion {
    #[allow(clippy::unnecessary_def_path)]
    fn init_default_impl_for_type_if_needed(&mut self, cx: &LateContext<'_>) {
        if self.default_impl_for_type.is_empty()
            && let Some(default_trait_id) = get_trait_def_id(cx, &["core", "default", "Default"])
        {
            let impls = cx.tcx.trait_impls_of(default_trait_id);
            for (ty, impl_def_ids) in impls.non_blanket_impls() {
                let Some(self_def_id) = ty.def() else { continue };
                for impl_def_id in impl_def_ids {
                    if !cx.tcx.has_attr(*impl_def_id, sym::automatically_derived) &&
                        let Some(assoc_item) = cx
                            .tcx
                            .associated_items(impl_def_id)
                            .in_definition_order()
                            // We're not interested in foreign implementations of the `Default` trait.
                            .find(|item| {
                                item.kind == AssocKind::Fn && item.def_id.is_local() && item.name == kw::Default
                            })
                        && let Some(body_node) = cx.tcx.hir().get_if_local(assoc_item.def_id)
                        && let Some(body_id) = body_node.body_id()
                        && let body = cx.tcx.hir().body(body_id)
                        // We don't want to keep it if it has conditional return.
                        && let [return_expr] = get_return_calls_in_body(body).as_slice()
                        && let ExprKind::Call(call_expr, _) = return_expr.kind
                        // We need to use typeck here to infer the actual function being called.
                        && let body_def_id = cx.tcx.hir().enclosing_body_owner(call_expr.hir_id)
                        && let Some(body_owner) = cx.tcx.hir().maybe_body_owned_by(body_def_id)
                        && let typeck = cx.tcx.typeck_body(body_owner)
                        && let Some(call_def_id) = typeck.type_dependent_def_id(call_expr.hir_id)
                    {
                        self.default_impl_for_type.insert(self_def_id, call_def_id);
                    }
                }
            }
        }
    }

    fn check_default_new<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        decl: &FnDecl<'tcx>,
        body: &'tcx Body<'tcx>,
        method_span: Span,
        method_def_id: LocalDefId,
    ) {
        // We're only interested into static methods.
        if decl.implicit_self.has_implicit_self() {
            return;
        }
        // We don't check trait implementations.
        if get_impl_trait_def_id(cx, method_def_id).is_some() {
            return;
        }

        let hir_id = cx.tcx.local_def_id_to_hir_id(method_def_id);
        if let Some((
            _,
            Node::Item(Item {
                kind: ItemKind::Impl(impl_),
                ..
            }),
        )) = cx.tcx.hir().parent_iter(hir_id).next()
            && let Some(implemented_ty_id) = get_hir_ty_def_id(cx.tcx, *impl_.self_ty)
            && {
                self.init_default_impl_for_type_if_needed(cx);
                true
            }
            && let Some(return_def_id) = self.default_impl_for_type.get(&implemented_ty_id)
            && method_def_id.to_def_id() == *return_def_id
        {
            let mut c = CheckCalls {
                cx,
                map: cx.tcx.hir(),
                implemented_ty_id,
                found_default_call: false,
                method_span,
            };
            walk_body(&mut c, body);
        }
    }
}

impl<'tcx> LateLintPass<'tcx> for UnconditionalRecursion {
    fn check_fn(
        &mut self,
        cx: &LateContext<'tcx>,
        kind: FnKind<'tcx>,
        decl: &'tcx FnDecl<'tcx>,
        body: &'tcx Body<'tcx>,
        method_span: Span,
        method_def_id: LocalDefId,
    ) {
        // If the function is a method...
        if let FnKind::Method(name, _) = kind
            && let expr = expr_or_init(cx, body.value).peel_blocks()
            // Doesn't have a conditional return.
            && !has_conditional_return(body, expr)
        {
            if name.name == sym::eq || name.name == sym::ne {
                check_partial_eq(cx, method_span, method_def_id, name, expr);
            } else if name.name == sym::to_string {
                check_to_string(cx, method_span, method_def_id, name, expr);
            }
            self.check_default_new(cx, decl, body, method_span, method_def_id);
        }
    }
}