use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_middle::mir;
use rustc_middle::mir::visit::Visitor;
use rustc_middle::mir::*;
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_session::Session;

pub(crate) struct GenericConvergencePass;

impl<'tcx> mir::MirPass<'tcx> for GenericConvergencePass {
    fn is_enabled(&self, _sess: &Session) -> bool {
        true
    }

    fn run_pass(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        let mut visitor = ConvergenceVisitor::new(tcx, body);
        visitor.visit_body(body);

        tracing::debug!("run trampoline");
        tracing::debug!(flow = ?visitor.arg_flow, args = ?visitor.args, latest = ?visitor.get_latest_convergence_point());
    }
}

#[derive(Debug)]
struct ArgInfo<'tcx> {
    convergence_point: Option<Location>,
    converged_type: Option<Ty<'tcx>>,
}

struct ConvergenceVisitor<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'a Body<'tcx>,
    args: FxHashMap<Local, ArgInfo<'tcx>>,
    arg_flow: FxHashMap<Local, FxHashSet<Local>>,
    visited: FxHashSet<BasicBlock>,
}

impl<'a, 'tcx> ConvergenceVisitor<'a, 'tcx> {
    fn new(tcx: TyCtxt<'tcx>, body: &'a Body<'tcx>) -> Self {
        let mut args = FxHashMap::default();
        let mut arg_flow = FxHashMap::default();
        for arg in body.args_iter() {
            let arg_ty = body.local_decls[arg].ty;
            if is_generic(arg_ty) {
                args.insert(arg, ArgInfo { convergence_point: None, converged_type: None });
                arg_flow.insert(arg, FxHashSet::from_iter([arg]));
            }
        }
        ConvergenceVisitor { body, tcx, args, arg_flow, visited: FxHashSet::default() }
    }
    #[allow(rustc::potential_query_instability)]
    fn all_args_converged(&self) -> bool {
        self.args.values().all(|info| info.convergence_point.is_some())
    }
    #[allow(rustc::potential_query_instability)]
    #[allow(dead_code)]
    fn get_latest_convergence_point(&self) -> Option<Location> {
        self.args
            .values()
            .filter_map(|info| info.convergence_point)
            .max_by_key(|loc| (loc.block.index(), loc.statement_index))
    }
    #[allow(rustc::potential_query_instability)]
    fn update_arg_flow(&mut self, source: Local, destination: Local) {
        if let Some(source_set) = self.arg_flow.get(&source).cloned() {
            self.arg_flow.entry(destination).or_insert_with(FxHashSet::default).extend(source_set);
        }
    }
    #[allow(rustc::potential_query_instability)]
    #[allow(dead_code)]
    fn check_convergence(&mut self, local: Local, new_ty: Ty<'tcx>, location: Location) {
        if let Some(arg_set) = self.arg_flow.get(&local) {
            for &arg in arg_set {
                if let Some(arg_info) = self.args.get_mut(&arg) {
                    if !is_generic(new_ty) && Some(new_ty) != arg_info.converged_type {
                        arg_info.convergence_point = Some(location);
                        arg_info.converged_type = Some(new_ty);
                    }
                }
            }
        }
    }
}

impl<'a, 'tcx> Visitor<'tcx> for ConvergenceVisitor<'a, 'tcx> {
    fn visit_basic_block_data(&mut self, block: BasicBlock, data: &BasicBlockData<'tcx>) {
        if self.visited.contains(&block) || self.all_args_converged() {
            return;
        }
        self.visited.insert(block);

        for (statement_index, statement) in data.statements.iter().enumerate() {
            let location = Location { block, statement_index };
            self.visit_statement(statement, location);

            if self.all_args_converged() {
                return;
            }
        }

        let terminator = data.terminator();
        let location = Location { block, statement_index: data.statements.len() };
        self.visit_terminator(terminator, location);
    }

    fn visit_statement(&mut self, statement: &Statement<'tcx>, location: Location) {
        if let StatementKind::Assign(box (lhs, rhs)) = &statement.kind {
            match rhs {
                Rvalue::Use(Operand::Move(place)) | Rvalue::Use(Operand::Copy(place)) => {
                    self.update_arg_flow(place.local, lhs.local);
                }
                _ => {}
            }

            let new_type = lhs.ty(&self.body.local_decls, self.tcx).ty;
            self.check_convergence(lhs.local, new_type, location);
        }
    }

    fn visit_terminator(&mut self, terminator: &Terminator<'tcx>, location: Location) {
        match &terminator.kind {
            TerminatorKind::Call { args, destination, .. } => {
                for arg in args {
                    if let Operand::Move(place) | Operand::Copy(place) = arg.node {
                        self.update_arg_flow(place.local, destination.local);
                    }
                }
                let dest_ty = destination.ty(&self.body.local_decls, self.tcx).ty;
                self.check_convergence(destination.local, dest_ty, location);
            }
            _ => {}
        }
        self.super_terminator(terminator, location);
    }
}

fn is_generic<'tcx>(ty: Ty<'tcx>) -> bool {
    match ty.kind() {
        ty::Param(_) => true,
        ty::Alias(ty::Projection, p) => is_generic(p.self_ty()),
        _ => false,
    }
}
