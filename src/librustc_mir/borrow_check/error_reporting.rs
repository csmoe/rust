// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use borrow_check::WriteKind;
use rustc::middle::region::ScopeTree;
use rustc::mir::VarBindingForm;
use rustc::mir::{BindingForm, BorrowKind, ClearCrossCrate, Field, Local};
use rustc::mir::{LocalDecl, LocalKind, Location, Operand, Place, PlaceBase};
use rustc::mir::{PlaceElem, ProjectionElem, Rvalue, Statement, StatementKind};
use rustc::ty;
use rustc_data_structures::indexed_vec::Idx;
use rustc_data_structures::sync::Lrc;
use rustc_errors::DiagnosticBuilder;
use syntax_pos::Span;

use super::borrow_set::BorrowData;
use super::{Context, MirBorrowckCtxt};
use super::InitializationRequiringAction;

use dataflow::move_paths::MovePathIndex;
use dataflow::{FlowAtLocation, MovingOutStatements};
use util::borrowck_errors::{BorrowckErrors, Origin};

impl<'cx, 'gcx, 'tcx> MirBorrowckCtxt<'cx, 'gcx, 'tcx> {
    pub(super) fn report_use_of_moved_or_uninitialized(
        &mut self,
        context: Context,
        desired_action: InitializationRequiringAction,
        (place, span): (&Place<'tcx>, Span),
        mpi: MovePathIndex,
        curr_move_out: &FlowAtLocation<MovingOutStatements<'_, 'gcx, 'tcx>>,
    ) {
        let use_spans = self
            .move_spans(place, context.loc)
            .or_else(|| self.borrow_spans(span, context.loc));
        let span = use_spans.args_or_use();

        let mois = self.move_data.path_map[mpi]
            .iter()
            .filter(|moi| curr_move_out.contains(moi))
            .collect::<Vec<_>>();

        if mois.is_empty() {
            let root_place = place.elem_base(self.tcx, 0);

            if self.moved_error_reported.contains(&root_place.clone()) {
                debug!(
                    "report_use_of_moved_or_uninitialized place: error about {:?} suppressed",
                    root_place
                );
                return;
            }

            self.moved_error_reported.insert(root_place.clone());

            let item_msg = match self.describe_place_with_options(place, IncludingDowncast(true)) {
                Some(name) => format!("`{}`", name),
                None => "value".to_owned(),
            };
            let mut err = self.tcx.cannot_act_on_uninitialized_variable(
                span,
                desired_action.as_noun(),
                &self
                    .describe_place_with_options(place, IncludingDowncast(true))
                    .unwrap_or("_".to_owned()),
                Origin::Mir,
            );
            err.span_label(span, format!("use of possibly uninitialized {}", item_msg));

            use_spans.var_span_label(
                &mut err,
                format!("{} occurs due to use in closure", desired_action.as_noun()),
            );

            err.buffer(&mut self.errors_buffer);
        } else {
            let msg = ""; //FIXME: add "partially " or "collaterally "

            let mut err = self.tcx.cannot_act_on_moved_value(
                span,
                desired_action.as_noun(),
                msg,
                self.describe_place_with_options(&place, IncludingDowncast(true)),
                Origin::Mir,
            );

            let mut is_loop_move = false;
            for moi in &mois {
                let move_out = self.move_data.moves[**moi];
                let moved_place = &self.move_data.move_paths[move_out.path].place;

                let move_spans = self.move_spans(moved_place, move_out.source);
                let move_span = move_spans.args_or_use();

                let move_msg = if move_spans.for_closure() {
                    " into closure"
                } else {
                    ""
                };

                if span == move_span {
                    err.span_label(
                        span,
                        format!("value moved{} here in previous iteration of loop", move_msg),
                    );
                    is_loop_move = true;
                } else {
                    err.span_label(move_span, format!("value moved{} here", move_msg));
                    move_spans.var_span_label(&mut err, "variable moved due to use in closure");
                };
            }

            use_spans.var_span_label(
                &mut err,
                format!("{} occurs due to use in closure", desired_action.as_noun()),
            );

            if !is_loop_move {
                err.span_label(
                    span,
                    format!(
                        "value {} here after move",
                        desired_action.as_verb_in_past_tense()
                    ),
                );
            }

            if let Some(ty) = self.retrieve_type_for_place(place) {
                let needs_note = match ty.sty {
                    ty::TypeVariants::TyClosure(id, _) => {
                        let tables = self.tcx.typeck_tables_of(id);
                        let node_id = self.tcx.hir.as_local_node_id(id).unwrap();
                        let hir_id = self.tcx.hir.node_to_hir_id(node_id);
                        if let Some(_) = tables.closure_kind_origins().get(hir_id) {
                            false
                        } else {
                            true
                        }
                    }
                    _ => true,
                };

                if needs_note {
                    let mpi = self.move_data.moves[*mois[0]].path;
                    let place = &self.move_data.move_paths[mpi].place;

                    if let Some(ty) = self.retrieve_type_for_place(place) {
                        let note_msg = match self
                            .describe_place_with_options(place, IncludingDowncast(true))
                        {
                            Some(name) => format!("`{}`", name),
                            None => "value".to_owned(),
                        };

                        err.note(&format!(
                            "move occurs because {} has type `{}`, \
                             which does not implement the `Copy` trait",
                            note_msg, ty
                        ));
                    }
                }
            }

            err.buffer(&mut self.errors_buffer);
        }
    }

    pub(super) fn report_move_out_while_borrowed(
        &mut self,
        context: Context,
        (place, _span): (&Place<'tcx>, Span),
        borrow: &BorrowData<'tcx>,
    ) {
        let tcx = self.tcx;
        let value_msg = match self.describe_place(place) {
            Some(name) => format!("`{}`", name),
            None => "value".to_owned(),
        };
        let borrow_msg = match self.describe_place(&borrow.borrowed_place) {
            Some(name) => format!("`{}`", name),
            None => "value".to_owned(),
        };

        let borrow_spans = self.retrieve_borrow_spans(borrow);
        let borrow_span = borrow_spans.args_or_use();

        let move_spans = self.move_spans(place, context.loc);
        let span = move_spans.args_or_use();

        let mut err = tcx.cannot_move_when_borrowed(
            span,
            &self.describe_place(place).unwrap_or("_".to_owned()),
            Origin::Mir,
        );
        err.span_label(borrow_span, format!("borrow of {} occurs here", borrow_msg));
        err.span_label(span, format!("move out of {} occurs here", value_msg));

        borrow_spans.var_span_label(&mut err, "borrow occurs due to use in closure");

        move_spans.var_span_label(&mut err, "move occurs due to use in closure");

        self.explain_why_borrow_contains_point(context, borrow, None, &mut err);
        err.buffer(&mut self.errors_buffer);
    }

    pub(super) fn report_use_while_mutably_borrowed(
        &mut self,
        context: Context,
        (place, _span): (&Place<'tcx>, Span),
        borrow: &BorrowData<'tcx>,
    ) {
        let tcx = self.tcx;

        let borrow_spans = self.retrieve_borrow_spans(borrow);
        let borrow_span = borrow_spans.args_or_use();

        // Conflicting borrows are reported separately, so only check for move
        // captures.
        let use_spans = self.move_spans(place, context.loc);
        let span = use_spans.var_or_use();

        let mut err = tcx.cannot_use_when_mutably_borrowed(
            span,
            &self.describe_place(place).unwrap_or("_".to_owned()),
            borrow_span,
            &self
                .describe_place(&borrow.borrowed_place)
                .unwrap_or("_".to_owned()),
            Origin::Mir,
        );

        borrow_spans.var_span_label(&mut err, {
            let place = &borrow.borrowed_place;
            let desc_place = self.describe_place(place).unwrap_or("_".to_owned());

            format!("borrow occurs due to use of `{}` in closure", desc_place)
        });

        self.explain_why_borrow_contains_point(context, borrow, None, &mut err);
        err.buffer(&mut self.errors_buffer);
    }

    pub(super) fn report_conflicting_borrow(
        &mut self,
        context: Context,
        (place, span): (&Place<'tcx>, Span),
        gen_borrow_kind: BorrowKind,
        issued_borrow: &BorrowData<'tcx>,
    ) {
        let issued_spans = self.retrieve_borrow_spans(issued_borrow);
        let issued_span = issued_spans.args_or_use();

        let borrow_spans = self.borrow_spans(span, context.loc);
        let span = borrow_spans.args_or_use();

        let desc_place = self.describe_place(place).unwrap_or("_".to_owned());
        let tcx = self.tcx;

        // FIXME: supply non-"" `opt_via` when appropriate
        let mut err = match (
            gen_borrow_kind,
            "immutable",
            "mutable",
            issued_borrow.kind,
            "immutable",
            "mutable",
        ) {
            (BorrowKind::Shared, lft, _, BorrowKind::Mut { .. }, _, rgt)
            | (BorrowKind::Mut { .. }, _, lft, BorrowKind::Shared, rgt, _) => tcx
                .cannot_reborrow_already_borrowed(
                    span,
                    &desc_place,
                    "",
                    lft,
                    issued_span,
                    "it",
                    rgt,
                    "",
                    None,
                    Origin::Mir,
                ),

            (BorrowKind::Mut { .. }, _, _, BorrowKind::Mut { .. }, _, _) => tcx
                .cannot_mutably_borrow_multiply(
                    span,
                    &desc_place,
                    "",
                    issued_span,
                    "",
                    None,
                    Origin::Mir,
                ),

            (BorrowKind::Unique, _, _, BorrowKind::Unique, _, _) => tcx
                .cannot_uniquely_borrow_by_two_closures(
                    span,
                    &desc_place,
                    issued_span,
                    None,
                    Origin::Mir,
                ),

            (BorrowKind::Unique, _, _, _, _, _) => tcx.cannot_uniquely_borrow_by_one_closure(
                span,
                &desc_place,
                "",
                issued_span,
                "it",
                "",
                None,
                Origin::Mir,
            ),

            (BorrowKind::Shared, lft, _, BorrowKind::Unique, _, _) => tcx
                .cannot_reborrow_already_uniquely_borrowed(
                    span,
                    &desc_place,
                    "",
                    lft,
                    issued_span,
                    "",
                    None,
                    Origin::Mir,
                ),

            (BorrowKind::Mut { .. }, _, lft, BorrowKind::Unique, _, _) => tcx
                .cannot_reborrow_already_uniquely_borrowed(
                    span,
                    &desc_place,
                    "",
                    lft,
                    issued_span,
                    "",
                    None,
                    Origin::Mir,
                ),

            (BorrowKind::Shared, _, _, BorrowKind::Shared, _, _) => unreachable!(),
        };

        if issued_spans == borrow_spans {
            borrow_spans.var_span_label(
                &mut err,
                format!(
                    "borrows occur due to use of `{}` in closure",
                    desc_place
                ),
            );
        } else {
            let borrow_place = &issued_borrow.borrowed_place;
            let borrow_place_desc = self.describe_place(borrow_place).unwrap_or("_".to_owned());
            issued_spans.var_span_label(
                &mut err,
                format!(
                    "first borrow occurs due to use of `{}` in closure",
                    borrow_place_desc
                ),
            );

            borrow_spans.var_span_label(
                &mut err,
                format!("second borrow occurs due to use of `{}` in closure", desc_place),
            );
        }

        self.explain_why_borrow_contains_point(context, issued_borrow, None, &mut err);

        err.buffer(&mut self.errors_buffer);
    }

    pub(super) fn report_borrowed_value_does_not_live_long_enough(
        &mut self,
        context: Context,
        borrow: &BorrowData<'tcx>,
        place_span: (&Place<'tcx>, Span),
        kind: Option<WriteKind>,
    ) {
        let drop_span = place_span.1;
        let scope_tree = self.tcx.region_scope_tree(self.mir_def_id);
        let root_place = borrow.borrowed_place.elem_base(self.tcx, 0);

        let borrow_spans = self.retrieve_borrow_spans(borrow);
        let borrow_span = borrow_spans.var_or_use();

        let proper_span = match root_place.base {
            PlaceBase::Local(local) if root_place.has_no_projection()
                => self.mir.local_decls[local].source_info.span,
            _ => drop_span,
        };

        if self
            .access_place_error_reported
            .contains(&(root_place.clone(), borrow_span))
        {
            debug!(
                "suppressing access_place error when borrow doesn't live long enough for {:?}",
                borrow_span
            );
            return;
        }

        self.access_place_error_reported
            .insert((root_place.clone(), borrow_span));

        let mut err = match &self.describe_place(&borrow.borrowed_place) {
            Some(name) => self.report_local_value_does_not_live_long_enough(
                context,
                name,
                &scope_tree,
                &borrow,
                drop_span,
                borrow_span,
                proper_span,
                kind.map(|k| (k, place_span.0)),
            ),
            None => self.report_temporary_value_does_not_live_long_enough(
                context,
                &scope_tree,
                &borrow,
                drop_span,
                borrow_span,
                proper_span,
            ),
        };

        borrow_spans.args_span_label(&mut err, "value captured here");

        err.buffer(&mut self.errors_buffer);
    }

    fn report_local_value_does_not_live_long_enough(
        &mut self,
        context: Context,
        name: &String,
        scope_tree: &Lrc<ScopeTree>,
        borrow: &BorrowData<'tcx>,
        drop_span: Span,
        borrow_span: Span,
        _proper_span: Span,
        kind_place: Option<(WriteKind, &Place<'tcx>)>,
    ) -> DiagnosticBuilder<'cx> {
        debug!(
            "report_local_value_does_not_live_long_enough(\
             {:?}, {:?}, {:?}, {:?}, {:?}, {:?}\
             )",
            context, name, scope_tree, borrow, drop_span, borrow_span
        );

        let tcx = self.tcx;
        let mut err =
            tcx.path_does_not_live_long_enough(borrow_span, &format!("`{}`", name), Origin::Mir);
        err.span_label(borrow_span, "borrowed value does not live long enough");
        err.span_label(
            drop_span,
            format!("`{}` dropped here while still borrowed", name),
        );

        self.explain_why_borrow_contains_point(context, borrow, kind_place, &mut err);
        err
    }

    fn report_temporary_value_does_not_live_long_enough(
        &mut self,
        context: Context,
        scope_tree: &Lrc<ScopeTree>,
        borrow: &BorrowData<'tcx>,
        drop_span: Span,
        _borrow_span: Span,
        proper_span: Span,
    ) -> DiagnosticBuilder<'cx> {
        debug!(
            "report_temporary_value_does_not_live_long_enough(\
             {:?}, {:?}, {:?}, {:?}, {:?}\
             )",
            context, scope_tree, borrow, drop_span, proper_span
        );

        let tcx = self.tcx;
        let mut err =
            tcx.path_does_not_live_long_enough(proper_span, "borrowed value", Origin::Mir);
        err.span_label(proper_span, "temporary value does not live long enough");
        err.span_label(drop_span, "temporary value only lives until here");

        self.explain_why_borrow_contains_point(context, borrow, None, &mut err);
        err
    }

    pub(super) fn report_illegal_mutation_of_borrowed(
        &mut self,
        context: Context,
        (place, span): (&Place<'tcx>, Span),
        loan: &BorrowData<'tcx>,
    ) {
        let loan_spans = self.retrieve_borrow_spans(loan);
        let loan_span = loan_spans.args_or_use();

        let tcx = self.tcx;
        let mut err = tcx.cannot_assign_to_borrowed(
            span,
            loan_span,
            &self.describe_place(place).unwrap_or("_".to_owned()),
            Origin::Mir,
        );

        loan_spans.var_span_label(&mut err, "borrow occurs due to use in closure");

        self.explain_why_borrow_contains_point(context, loan, None, &mut err);

        err.buffer(&mut self.errors_buffer);
    }

    /// Reports an illegal reassignment; for example, an assignment to
    /// (part of) a non-`mut` local that occurs potentially after that
    /// local has already been initialized. `place` is the path being
    /// assigned; `err_place` is a place providing a reason why
    /// `place` is not mutable (e.g. the non-`mut` local `x` in an
    /// assignment to `x.f`).
    pub(super) fn report_illegal_reassignment(
        &mut self,
        _context: Context,
        (place, span): (&Place<'tcx>, Span),
        assigned_span: Span,
        err_place: &Place<'tcx>,
    ) {
        let (mut from_arg, mut local_decl) = (false, None);

        if let PlaceBase::Local(local) = err_place.base {
            if err_place.has_no_projection() {
                if let LocalKind::Arg = self.mir.local_kind(local) {
                    from_arg = true;
                    local_decl = Some(&self.mir.local_decls[local]);
                } else {
                    local_decl = Some(&self.mir.local_decls[local]);
                }
            }
        }

        // If root local is initialized immediately (everything apart from let
        // PATTERN;) then make the error refer to that local, rather than the
        // place being assigned later.
        let (place_description, assigned_span) = match local_decl {
            Some(LocalDecl {
                is_user_variable: Some(ClearCrossCrate::Clear),
                ..
            })
            | Some(LocalDecl {
                is_user_variable:
                    Some(ClearCrossCrate::Set(BindingForm::Var(VarBindingForm {
                        opt_match_place: None,
                        ..
                    }))),
                ..
            })
            | Some(LocalDecl {
                is_user_variable: None,
                ..
            })
            | None => (self.describe_place(place), assigned_span),
            Some(decl) => (self.describe_place(err_place), decl.source_info.span),
        };

        let mut err = self.tcx.cannot_reassign_immutable(
            span,
            place_description.as_ref().map(AsRef::as_ref).unwrap_or("_"),
            from_arg,
            Origin::Mir,
        );
        let msg = if from_arg {
            "cannot assign to immutable argument"
        } else {
            "cannot assign twice to immutable variable"
        };
        if span != assigned_span {
            if !from_arg {
                let value_msg = match place_description {
                    Some(name) => format!("`{}`", name),
                    None => "value".to_owned(),
                };
                err.span_label(assigned_span, format!("first assignment to {}", value_msg));
            }
        }
        if let Some(decl) = local_decl {
            if let Some(name) = decl.name {
                if decl.can_be_made_mutable() {
                    err.span_label(
                        decl.source_info.span,
                        format!("consider changing this to `mut {}`", name),
                    );
                }
            }
        }
        err.span_label(span, msg);
        err.buffer(&mut self.errors_buffer);
    }
}

pub(super) struct IncludingDowncast(bool);

impl<'cx, 'gcx, 'tcx> MirBorrowckCtxt<'cx, 'gcx, 'tcx> {
    // End-user visible description of `place` if one can be found. If the
    // place is a temporary for instance, None will be returned.
    pub(super) fn describe_place(&self, place: &Place<'tcx>) -> Option<String> {
        self.describe_place_with_options(place, IncludingDowncast(false))
    }

    // End-user visible description of `place` if one can be found. If the
    // place is a temporary for instance, None will be returned.
    // `IncludingDowncast` parameter makes the function return `Err` if `ProjectionElem` is
    // `Downcast` and `IncludingDowncast` is true
    pub(super) fn describe_place_with_options(
        &self,
        place: &Place<'tcx>,
        including_downcast: IncludingDowncast,
    ) -> Option<String> {
        let mut buf = String::new();
        match self.append_place_to_string(place, &mut buf, &including_downcast) {
            Ok(()) => Some(buf),
            Err(()) => None,
        }
    }

    // Appends end-user visible description of `place` to `buf`.
    fn append_place_to_string(
        &self,
        place: &Place<'tcx>,
        buf: &mut String,
        including_downcast: &IncludingDowncast,
    ) -> Result<(), ()> {
        self.append_place_base_to_string(&place.base, buf)?;
        if !place.has_no_projection() {
            for elem in place.elems.iter() {
                self.append_place_projection_to_string(place, elem, buf, including_downcast)?;
            }
        }
        Ok(())
    }

    fn append_place_base_to_string(
        &self,
        base: &PlaceBase<'tcx>,
        buf: &mut String,
    ) -> Result<(), ()> {
        match base {
            PlaceBase::Promoted(_) => {
                buf.push_str("promoted");
            }
            PlaceBase::Local(local) => {
                self.append_local_to_string(*local, buf)?;
            }
            PlaceBase::Static(ref static_) => {
                buf.push_str(&self.tcx.item_name(static_.def_id).to_string());
            }
        }
        Ok(())
    }

    fn append_place_projection_to_string(
        &self,
        place: &Place<'tcx>,
        projection: &PlaceElem<'tcx>,
        buf: &mut String,
        including_downcast: &IncludingDowncast,
    ) -> Result<(), ()> {
        match projection {
            ProjectionElem::Deref => {
                let upvar_field_projection =
                    place.is_upvar_field_projection(self.mir, &self.tcx);
                if let Some(field) = upvar_field_projection {
                    let var_index = field.index();
                    let name = self.mir.upvar_decls[var_index].debug_name.as_str();
                    if self.mir.upvar_decls[var_index].by_ref {
                        buf.push_str(&name);
                    } else {
                        buf.push_str(&format!("*{}", name));
                    }
                } else {
                    buf.push_str(&"*");
                }
            }
            ProjectionElem::Downcast(..) => {
                if including_downcast.0 {
                    return Err(());
                }
            }
            ProjectionElem::Field(field, _ty) => {
                let upvar_field_projection =
                    place.is_upvar_field_projection(self.mir, &self.tcx);
                if let Some(field) = upvar_field_projection {
                    let var_index = field.index();
                    let name = self.mir.upvar_decls[var_index].debug_name.as_str();
                    buf.push_str(&name);
                } else {
                    let field_name = self.describe_field(&place, *field);
                    buf.push_str(&format!(".{}", field_name));
                }
            }
            ProjectionElem::Index(index) => {
                buf.push_str("[");
                if let Err(_) = self.append_local_to_string(*index, buf) {
                    buf.push_str("..");
                }
                buf.push_str("]");
            }
            ProjectionElem::ConstantIndex { .. } | ProjectionElem::Subslice { .. } => {
                buf.push_str(&"[..]");
            }
        }
        Ok(())
    }

    // Appends end-user visible description of the `local` place to `buf`. If `local` doesn't have
    // a name, then `Err` is returned
    fn append_local_to_string(&self, local_index: Local, buf: &mut String) -> Result<(), ()> {
        let local = &self.mir.local_decls[local_index];
        match local.name {
            Some(name) => {
                buf.push_str(&name.to_string());
                Ok(())
            }
            None => Err(()),
        }
    }

    // End-user visible description of the `field`nth field of `base`
    fn describe_field(&self, place: &Place<'tcx>, field: Field) -> String {
        let mut string: String = match place.base {
             PlaceBase::Local(local) => {
                 let local = &self.mir.local_decls[local];
                 self.describe_field_from_ty(&local.ty, field)
             }
             PlaceBase::Promoted(ref prom) => self.describe_field_from_ty(&prom.1, field),
             PlaceBase::Static(ref static_) => self.describe_field_from_ty(&static_.ty, field),
        };
        if !place.has_no_projection() {
            for elem in place.elems.iter() {
                let proj_str = match elem {
                    ProjectionElem::Downcast(def, variant_index) => format!(
                        "{}",
                        def.variants[*variant_index].fields[field.index()].ident
                    ).to_string(),
                    ProjectionElem::Field(_, field_type) => {
                        self.describe_field_from_ty(&field_type, field).to_string()
                    }
                    ProjectionElem::Index(..)
                    | ProjectionElem::Deref
                    | ProjectionElem::ConstantIndex { .. }
                    | ProjectionElem::Subslice { .. } => continue,
                };
                string.push_str(proj_str.as_str());
            }
        }
        string
    }

    // End-user visible description of the `field_index`nth field of `ty`
    fn describe_field_from_ty(&self, ty: &ty::Ty, field: Field) -> String {
        if ty.is_box() {
            // If the type is a box, the field is described from the boxed type
            self.describe_field_from_ty(&ty.boxed_ty(), field)
        } else {
            match ty.sty {
                ty::TyAdt(def, _) => if def.is_enum() {
                    field.index().to_string()
                } else {
                    def.non_enum_variant().fields[field.index()]
                        .ident
                        .to_string()
                },
                ty::TyTuple(_) => field.index().to_string(),
                ty::TyRef(_, ty, _) | ty::TyRawPtr(ty::TypeAndMut { ty, .. }) => {
                    self.describe_field_from_ty(&ty, field)
                }
                ty::TyArray(ty, _) | ty::TySlice(ty) => self.describe_field_from_ty(&ty, field),
                ty::TyClosure(def_id, _) | ty::TyGenerator(def_id, _, _) => {
                    // Convert the def-id into a node-id. node-ids are only valid for
                    // the local code in the current crate, so this returns an `Option` in case
                    // the closure comes from another crate. But in that case we wouldn't
                    // be borrowck'ing it, so we can just unwrap:
                    let node_id = self.tcx.hir.as_local_node_id(def_id).unwrap();
                    let freevar = self.tcx.with_freevars(node_id, |fv| fv[field.index()]);

                    self.tcx.hir.name(freevar.var_id()).to_string()
                }
                _ => {
                    // Might need a revision when the fields in trait RFC is implemented
                    // (https://github.com/rust-lang/rfcs/pull/1546)
                    bug!(
                        "End-user description not implemented for field access on `{:?}`",
                        ty.sty
                    );
                }
            }
        }
    }

    // Retrieve type of a place for the current MIR representation
    fn retrieve_type_for_place(&self, place: &Place<'tcx>) -> Option<ty::Ty> {
        let place = place.clone();
        let mut ty = match place.base {
            PlaceBase::Local(local) => {
                let local = &self.mir.local_decls[local];
                Some(local.ty)
            }
            PlaceBase::Promoted(prom) => Some(prom.1),
            PlaceBase::Static(st) => Some(st.ty),
        };
        if let Some(projection) = place.elems.last() {
            ty = match projection {
                 ProjectionElem::Field(_, ty) => Some(ty),
                 _ => None,
            };
        }
        ty
    }
}

// The span(s) associated to a use of a place.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(super) enum UseSpans {
    // The access is caused by capturing a variable for a closure.
    ClosureUse {
        // The span of the args of the closure, including the `move` keyword if
        // it's present.
        args_span: Span,
        // The span of the first use of the captured variable inside the closure.
        var_span: Span
    },
    // This access has a single span associated to it: common case.
    OtherUse(Span),
}

impl UseSpans {
    pub(super) fn args_or_use(self) -> Span {
        match self {
            UseSpans::ClosureUse {
                args_span: span, ..
            }
            | UseSpans::OtherUse(span) => span,
        }
    }

    pub(super) fn var_or_use(self) -> Span {
        match self {
            UseSpans::ClosureUse { var_span: span, .. } | UseSpans::OtherUse(span) => span,
        }
    }

    // Add a span label to the arguments of the closure, if it exists.
    pub(super) fn args_span_label(self, err: &mut DiagnosticBuilder, message: impl Into<String>) {
        if let UseSpans::ClosureUse { args_span, .. } = self {
            err.span_label(args_span, message);
        }
    }

    // Add a span label to the use of the captured variable, if it exists.
    pub(super) fn var_span_label(self, err: &mut DiagnosticBuilder, message: impl Into<String>) {
        if let UseSpans::ClosureUse { var_span, .. } = self {
            err.span_label(var_span, message);
        }
    }

    pub(super) fn for_closure(self) -> bool {
        match self {
            UseSpans::ClosureUse { .. } => true,
            UseSpans::OtherUse(_) => false,
        }
    }

    pub(super) fn or_else<F>(self, if_other: F) -> Self
    where
        F: FnOnce() -> Self,
    {
        match self {
            closure @ UseSpans::ClosureUse { .. } => closure,
            UseSpans::OtherUse(_) => if_other(),
        }
    }
}

impl<'cx, 'gcx, 'tcx> MirBorrowckCtxt<'cx, 'gcx, 'tcx> {
    /// Finds the spans associated to a move or copy of move_place at location.
    pub(super) fn move_spans(
        &self,
        moved_place: &Place<'tcx>, // Could also be an upvar.
        location: Location,
    ) -> UseSpans {
        use self::UseSpans::*;
        use rustc::hir::ExprKind::Closure;
        use rustc::mir::AggregateKind;

        let stmt = match self.mir[location.block]
            .statements
            .get(location.statement_index)
        {
            Some(stmt) => stmt,
            None => return OtherUse(self.mir.source_info(location).span),
        };

        if let StatementKind::Assign(_, Rvalue::Aggregate(ref kind, ref places)) = stmt.kind {
            if let AggregateKind::Closure(def_id, _) = **kind {
                debug!("find_closure_move_span: found closure {:?}", places);

                if let Some(node_id) = self.tcx.hir.as_local_node_id(def_id) {
                    if let Closure(_, _, _, args_span, _) = self.tcx.hir.expect_expr(node_id).node {
                        if let Some(var_span) = self.tcx.with_freevars(node_id, |freevars| {
                            for (v, place) in freevars.iter().zip(places) {
                                match place {
                                    Operand::Copy(place) | Operand::Move(place)
                                        if moved_place == place =>
                                    {
                                        debug!(
                                            "find_closure_move_span: found captured local {:?}",
                                            place
                                        );
                                        return Some(v.span);
                                    }
                                    _ => {}
                                }
                            }
                            None
                        }) {
                            return ClosureUse {
                                args_span,
                                var_span,
                            };
                        }
                    }
                }
            }
        }

        return OtherUse(stmt.source_info.span);
    }

    /// Finds the span of arguments of a closure (within `maybe_closure_span`)
    /// and its usage of the local assigned at `location`.
    /// This is done by searching in statements succeeding `location`
    /// and originating from `maybe_closure_span`.
    pub(super) fn borrow_spans(&self, use_span: Span, location: Location) -> UseSpans {
        use self::UseSpans::*;
        use rustc::hir::ExprKind::Closure;
        use rustc::mir::AggregateKind;

        let local = match self.mir[location.block]
            .statements
            .get(location.statement_index)
        {
            Some(&Statement {
                kind: StatementKind::Assign(
                    Place {
                        base: PlaceBase::Local(local),
                        elems: _,
                    },
                    _,
                ),
                ..
            }) => local,
            _ => return OtherUse(use_span),
        };

        if self.mir.local_kind(local) != LocalKind::Temp {
            // operands are always temporaries.
            return OtherUse(use_span);
        }

        for stmt in &self.mir[location.block].statements[location.statement_index + 1..] {
            if let StatementKind::Assign(_, Rvalue::Aggregate(ref kind, ref places)) = stmt.kind {
                if let AggregateKind::Closure(def_id, _) = **kind {
                    debug!("find_closure_borrow_span: found closure {:?}", places);

                    return if let Some(node_id) = self.tcx.hir.as_local_node_id(def_id) {
                        let args_span = if let Closure(_, _, _, span, _) =
                            self.tcx.hir.expect_expr(node_id).node
                        {
                            span
                        } else {
                            return OtherUse(use_span);
                        };

                        self.tcx
                            .with_freevars(node_id, |freevars| {
                                for (v, place) in freevars.iter().zip(places) {
                                    match place {
                                        Operand::Copy(place)
                                        | Operand::Move(place)
                                            if PlaceBase::Local(local) == place.base &&
                                                place.has_no_projection() =>
                                        {
                                            debug!(
                                                "find_closure_borrow_span: found captured local \
                                                 {:?}",
                                                local
                                            );
                                            return Some(v.span);
                                        }
                                        _ => {}
                                    }
                                }
                                None
                            }).map(|var_span| ClosureUse {
                                args_span,
                                var_span,
                            }).unwrap_or(OtherUse(use_span))
                    } else {
                        OtherUse(use_span)
                    };
                }
            }

            if use_span != stmt.source_info.span {
                break;
            }
        }

        OtherUse(use_span)
    }

    /// Helper to retrieve span(s) of given borrow from the current MIR
    /// representation
    pub(super) fn retrieve_borrow_spans(&self, borrow: &BorrowData) -> UseSpans {
        let span = self.mir.source_info(borrow.reserve_location).span;
        self.borrow_spans(span, borrow.reserve_location)
    }
}
