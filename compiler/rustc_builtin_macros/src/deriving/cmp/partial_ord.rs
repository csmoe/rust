use crate::deriving::generic::ty::*;
use crate::deriving::generic::*;
use crate::deriving::{path_std, pathvec_std};

use rustc_ast::MetaItem;
use rustc_expand::base::{Annotatable, ExtCtxt};
use rustc_span::symbol::{sym, Ident};
use rustc_span::Span;

pub fn expand_deriving_partial_ord(
    cx: &mut ExtCtxt<'_>,
    span: Span,
    mitem: &MetaItem,
    item: &Annotatable,
    push: &mut dyn FnMut(Annotatable),
) {
    let ordering_ty = Path(path_std!(cmp::Ordering));
    let ret_ty =
        Path(Path::new_(pathvec_std!(option::Option), vec![Box::new(ordering_ty)], PathKind::Std));

    let inline = cx.meta_word(span, sym::inline);
    let attrs = vec![cx.attribute(inline)];

    let partial_cmp_def = MethodDef {
        name: sym::partial_cmp,
        generics: Bounds::empty(),
        explicit_self: true,
        nonself_args: vec![(self_ref(), sym::other)],
        ret_ty,
        attributes: attrs,
        unify_fieldless_variants: true,
        combine_substructure: combine_substructure(Box::new(|cx, span, substr| {
            cs_partial_cmp(cx, span, substr)
        })),
    };

    let trait_def = TraitDef {
        span,
        attributes: vec![],
        path: path_std!(cmp::PartialOrd),
        additional_bounds: vec![],
        generics: Bounds::empty(),
        supports_unions: false,
        methods: vec![partial_cmp_def],
        associated_types: Vec::new(),
    };
    trait_def.expand(cx, mitem, item, push)
}

pub fn cs_partial_cmp(cx: &mut ExtCtxt<'_>, span: Span, substr: &Substructure<'_>) -> BlockOrExpr {
    let test_id = Ident::new(sym::cmp, span);
    let equal_path = cx.path_global(span, cx.std_path(&[sym::cmp, sym::Ordering, sym::Equal]));
    let partial_cmp_path = cx.std_path(&[sym::cmp, sym::PartialOrd, sym::partial_cmp]);

    // Builds:
    //
    // match ::core::cmp::PartialOrd::partial_cmp(&self.x, &other.x) {
    //     ::core::option::Option::Some(::core::cmp::Ordering::Equal) =>
    //         ::core::cmp::PartialOrd::partial_cmp(&self.y, &other.y),
    //     cmp => cmp,
    // }
    let expr = cs_fold(
        // foldr nests the if-elses correctly, leaving the first field
        // as the outermost one, and the last as the innermost.
        false,
        |cx, span, old, self_expr, other_selflike_exprs| {
            // match new {
            //     Some(::core::cmp::Ordering::Equal) => old,
            //     cmp => cmp
            // }
            let new = {
                let [other_expr] = other_selflike_exprs else {
                    cx.span_bug(span, "not exactly 2 arguments in `derive(PartialOrd)`");
                };

                let args = vec![
                    cx.expr_addr_of(span, self_expr),
                    cx.expr_addr_of(span, other_expr.clone()),
                ];

                cx.expr_call_global(span, partial_cmp_path.clone(), args)
            };

            let eq_arm =
                cx.arm(span, cx.pat_some(span, cx.pat_path(span, equal_path.clone())), old);
            let neq_arm = cx.arm(span, cx.pat_ident(span, test_id), cx.expr_ident(span, test_id));

            cx.expr_match(span, new, vec![eq_arm, neq_arm])
        },
        |cx, args| match args {
            Some((span, self_expr, other_selflike_exprs)) => {
                let new = {
                    let [other_expr] = other_selflike_exprs else {
                            cx.span_bug(span, "not exactly 2 arguments in `derive(Ord)`");
                        };
                    let args = vec![
                        cx.expr_addr_of(span, self_expr),
                        cx.expr_addr_of(span, other_expr.clone()),
                    ];
                    cx.expr_call_global(span, partial_cmp_path.clone(), args)
                };

                new
            }
            None => cx.expr_some(span, cx.expr_path(equal_path.clone())),
        },
        Box::new(|cx, span, tag_tuple| {
            if tag_tuple.len() != 2 {
                cx.span_bug(span, "not exactly 2 arguments in `derive(PartialOrd)`")
            } else {
                let lft = cx.expr_addr_of(span, cx.expr_ident(span, tag_tuple[0]));
                let rgt = cx.expr_addr_of(span, cx.expr_ident(span, tag_tuple[1]));
                let fn_partial_cmp_path =
                    cx.std_path(&[sym::cmp, sym::PartialOrd, sym::partial_cmp]);
                cx.expr_call_global(span, fn_partial_cmp_path, vec![lft, rgt])
            }
        }),
        cx,
        span,
        substr,
    );
    BlockOrExpr::new_expr(expr)
}
