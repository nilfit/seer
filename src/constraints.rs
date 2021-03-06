use rustc::mir;
use z3;

use memory::{AbstractVariable, SByte};
use value::{PrimVal, PrimValKind};

#[derive(Debug, Clone, Copy)]
pub enum NumericIntrinsic {
    Ctpop,
    Ctlz,
    Cttz,
}

#[derive(Debug, Clone, Copy)]
enum VarType {
    Bool,
    BitVec8,
    Array, // Array of BitVec8, indexed by BitVec64?
}

impl VarType {
    fn from_prim_val_kind(kind: PrimValKind) -> Self {
        use value::PrimValKind::*;
        match kind {
            Bool => VarType::Bool,
            U8 | I8 => VarType::BitVec8,
            U16 | I16 => VarType::BitVec8,
            U32 | I32 => VarType::BitVec8,
            U64 | I64 => VarType::BitVec8,
            _ => unimplemented!(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum VarOrigin {
    StdIn, // abstract byte read from stdin
    Inner, // anything else
}

#[derive(Clone, Debug)]
pub struct ConstraintContext {
    /// Each entry represents a variable. The index is the variable ID and
    /// the value is the variable's type.
    variables: Vec<(VarType, VarOrigin)>,

    constraints: Vec<Constraint>,
}

#[derive(Clone, Copy, Debug)]
pub enum Constraint {
    Binop {
        operator: mir::BinOp,
        kind: PrimValKind,
        rhs_operand1: PrimVal,
        rhs_operand2: PrimVal,
        lhs: PrimVal,
        lhs_kind: PrimValKind,
    },

    // lhs = op(rhs)
    Unop {
        operator: mir::UnOp,
        kind: PrimValKind,
        operand: PrimVal,
        lhs: PrimVal,
    },

    Compare { op: mir::BinOp, kind: PrimValKind, lhs: PrimVal, rhs: PrimVal, },

    NumericIntrinsic {
        operator: NumericIntrinsic,
        kind: PrimValKind,
        operand: PrimVal,
        lhs: PrimVal,
    },

    // lhs = if discriminant then then_branch else else_branch
    IfThenElse {
        discriminant: PrimVal,
        kind: PrimValKind,
        then_branch: PrimVal,
        else_branch: PrimVal,
        lhs: PrimVal,
    },

    // array[index] = value
    ArrayElement {
        array: AbstractVariable,
        index: PrimVal,
        value: SByte,
    },

    // lhs = array.store(idx, value)
    ArrayStore {
        array: AbstractVariable,
        index: PrimVal,
        value: SByte,
        lhs: AbstractVariable,
    }
}

impl Constraint {
    pub fn new_binop(
        operator: mir::BinOp,
        kind: PrimValKind,
        rhs_operand1: PrimVal,
        rhs_operand2: PrimVal,
        lhs: PrimVal,
        lhs_kind: PrimValKind,
    ) -> Self {
        Constraint::Binop {
            operator, kind, rhs_operand1, rhs_operand2, lhs, lhs_kind,
        }
    }

    pub fn new_unop(
        operator: mir::UnOp,
        kind: PrimValKind,
        operand: PrimVal,
        lhs: PrimVal,
    ) -> Self {
        Constraint::Unop {
            operator, kind, operand, lhs,
        }
    }

    pub fn new_intrinsic(
        operator: NumericIntrinsic,
        kind: PrimValKind,
        operand: PrimVal,
        lhs: PrimVal,
    ) -> Self {
        Constraint::NumericIntrinsic {
            operator, kind, operand, lhs,
        }
    }

    pub fn new_compare(op: mir::BinOp, kind: PrimValKind, lhs: PrimVal, rhs: PrimVal) -> Self {
        Constraint::Compare { op, kind, lhs, rhs }
    }
}

impl ConstraintContext {
    pub fn new() -> Self {
        ConstraintContext {
            variables: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn allocate_abstract_var(&mut self, var_type: VarType, origin: VarOrigin) -> AbstractVariable {
        let id = self.variables.len() as u32;
        self.variables.push((var_type, origin));
        AbstractVariable(id)
    }

    pub fn fresh_stdin_byte(&mut self) -> SByte {
        SByte::Abstract(self.allocate_abstract_var(VarType::BitVec8, VarOrigin::StdIn))
    }

    pub fn push_constraint(&mut self, constraint: Constraint) {
        self.constraints.push(constraint);
    }

    /// Creates a fresh abstract PrimVal `X` and adds a constraint
    /// `X == rhs_operand1 binop rhs_operand2`. Returns `X`.
    pub fn add_binop_constraint(
        &mut self,
        bin_op: mir::BinOp,
        rhs_operand1: PrimVal,
        rhs_operand2: PrimVal,
        kind: PrimValKind) -> PrimVal {

        use value::PrimValKind::*;

        let mut buffer = [SByte::Concrete(0); 8];

        let (num_bytes, var_type, lhs_kind) = match (bin_op, kind) {
            (mir::BinOp::Eq, _) |
            (mir::BinOp::Ne, _) |
            (mir::BinOp::Lt, _) |
            (mir::BinOp::Le, _) |
            (mir::BinOp::Gt, _) |
            (mir::BinOp::Ge, _) => (1, VarType::Bool, PrimValKind::Bool),
            (_, Bool) => (1, VarType::Bool, kind),
            (_, U8) | (_, I8) => (1, VarType::BitVec8, kind),
            (_, U16) | (_, I16) => (2, VarType::BitVec8, kind),
            (_, U32) | (_, I32) => (4, VarType::BitVec8, kind),
            (_, U64) | (_, I64) => (8, VarType::BitVec8, kind),
            _ => unimplemented!(),
        };

        for idx in 0..num_bytes {
            buffer[idx] = SByte::Abstract(self.allocate_abstract_var(var_type, VarOrigin::Inner));
        }

        let primval = PrimVal::Abstract(buffer);

        let constraint = Constraint::new_binop(bin_op, kind, rhs_operand1,
                                               rhs_operand2, primval, lhs_kind);

        self.push_constraint(constraint);

        primval
    }

    /// Creates a fresh abstract PrimVal `X` and adds a constraint
    /// `X == unop right`. Returns `X`.
    pub fn add_unop_constraint(
        &mut self,
        un_op: mir::UnOp,
        val: PrimVal,
        kind: PrimValKind) -> PrimVal {

        use value::PrimValKind::*;

        let (num_bytes, var_type) = match kind {
            Bool => (1, VarType::Bool),
            U8 | I8 => (1, VarType::BitVec8),
            _ => unimplemented!(),
        };

        let mut buffer = [SByte::Concrete(0); 8];
        for idx in 0..num_bytes {
            buffer[idx] = SByte::Abstract(self.allocate_abstract_var(var_type, VarOrigin::Inner));
        }

        let primval = PrimVal::Abstract(buffer);
        let constraint = Constraint::new_unop(un_op, kind, val, primval);

        self.push_constraint(constraint);

        primval
    }

    /// Creates a fresh abstract PrimVal `X` and adds a constraint
    /// `X == intrinsic(val)`. Returns `X`.
    pub fn add_intrinsic_constraint(
        &mut self,
        op: NumericIntrinsic,
        val: PrimVal,
        kind: PrimValKind) -> PrimVal {

        let num_bytes = kind.num_bytes();

        let mut buffer = [SByte::Concrete(0); 8];
        for idx in 0..num_bytes {
            buffer[idx] = SByte::Abstract(self.allocate_abstract_var(VarType::BitVec8, VarOrigin::Inner));
        }

        let primval = PrimVal::Abstract(buffer);
        let constraint = Constraint::new_intrinsic(op, kind, val, primval);

        self.push_constraint(constraint);

        primval
    }

    pub fn add_if_then_else(
        &mut self,
        discriminant: PrimVal,
        kind: PrimValKind,
        then_branch: PrimVal,
        else_branch: PrimVal
    ) -> PrimVal {
        let var_type = VarType::from_prim_val_kind(kind);

        let num_bytes = kind.num_bytes();
        let mut buffer = [SByte::Concrete(0); 8];
        for idx in 0..num_bytes {
            buffer[idx] = SByte::Abstract(self.allocate_abstract_var(var_type, VarOrigin::Inner));
        }

        let lhs = PrimVal::Abstract(buffer);
        self.push_constraint(Constraint::IfThenElse {
            discriminant,
            kind,
            then_branch,
            else_branch,
            lhs,
        });

        lhs
    }

    pub fn new_array(&mut self) -> AbstractVariable {
        self.allocate_abstract_var(VarType::Array, VarOrigin::Inner)
    }

    pub fn set_array_element_constraint(
        &mut self,
        array: AbstractVariable,
        index: PrimVal,
        value: SByte)
    {
        self.push_constraint(
            Constraint::ArrayElement {
                array, index, value,
            });
    }

    pub fn add_array_element_constraint(
        &mut self,
        array: AbstractVariable,
        index: PrimVal)
        -> SByte
    {
        let value = SByte::Abstract(self.allocate_abstract_var(VarType::BitVec8, VarOrigin::Inner));
        self.push_constraint(
            Constraint::ArrayElement {
                array, index, value,
            });

        value
    }

    pub fn store_array_element(
        &mut self,
        array: AbstractVariable,
        index: PrimVal,
        value: SByte)
        -> AbstractVariable
    {
        let new_array = self.new_array();
        self.push_constraint(
            Constraint::ArrayStore {
                array, index, value, lhs: new_array,
            });
        new_array
    }

    pub fn get_satisfying_values(&self) -> Vec<u8> {
        let cfg = z3::Config::new();
        let ctx = z3::Context::new(&cfg);
        let solver = z3::Solver::new(&ctx);

        let mut consts = Vec::new();

        let mut result_consts = Vec::new();

        for (idx, v) in self.variables.iter().enumerate() {
            let (var_type, var_origin) = *v;
            match var_type {
                VarType::Bool => {
                    consts.push(ctx.numbered_bool_const(idx as u32));
                }
                VarType::BitVec8 => {
                    consts.push(ctx.numbered_bitvector_const(idx as u32, 8));
                    if let VarOrigin::StdIn = var_origin {
                        result_consts.push(ctx.numbered_bitvector_const(idx as u32, 8));
                    }
                }
                VarType::Array => {
                    consts.push(
                        ::z3::Ast::new_const(
                            &::z3::Symbol::from_int(&ctx, idx as u32),
                            &ctx.array_sort(
                                &ctx.bitvector_sort(64),
                                &ctx.bitvector_sort(8))));
                }
            }

        }

        for c in &self.constraints {
            solver.assert(&self.constraint_to_ast(&ctx, *c));
        }

        assert!(solver.check());
        let model = solver.get_model();

        let mut result = Vec::new();
        for rc in result_consts {
            result.push(model.eval(&rc).unwrap().as_u64().unwrap() as u8);
        }

        result
    }

    pub fn is_feasible_with(
        &self,
        constraints: &[Constraint])
        -> bool
    {
        let cfg = z3::Config::new();
        let ctx = z3::Context::new(&cfg);
        let solver = z3::Solver::new(&ctx);

        let mut all_constraints: Vec<Constraint> = Vec::new();
        all_constraints.extend(self.constraints.iter().clone());
        all_constraints.extend(constraints.iter().clone());

        for c in all_constraints {
            solver.assert(&self.constraint_to_ast(&ctx, c));
        }

        solver.check()
    }

    fn sbyte_to_ast<'a>(
        &self,
        ctx: &'a z3::Context,
        sbyte: SByte)
        -> z3::Ast<'a>
    {
        match sbyte {
            SByte::Abstract(b) => {
                ctx.numbered_bitvector_const(b.0, 8)
            }
            SByte::Concrete(b) => {
                z3::Ast::bv_from_u64(&ctx, b as u64, 8)
            }
        }
    }

    fn sbyte_slice_to_ast<'a>(
        &self,
        ctx: &'a z3::Context,
        sbytes: &[SByte])
        -> z3::Ast<'a>
    {
        if sbytes.is_empty() {
            panic!("expected non-empty sbyte slice");
        } else if sbytes.len() == 1 {
            self.sbyte_to_ast(ctx, sbytes[0])
        } else {
            let mut result = self.sbyte_to_ast(ctx, sbytes[0]);
            for sbyte in &sbytes[1..] {
                result = self.sbyte_to_ast(ctx, *sbyte).concat(&result);
            }

            result
        }
    }

    fn constraint_to_ast<'a>(
        &self,
        ctx: &'a z3::Context,
        constraint: Constraint)
        -> z3::Ast<'a>
    {
        match constraint {
            Constraint::Binop { operator, kind, lhs, rhs_operand1,
                                rhs_operand2, lhs_kind } => {
                self.primval_to_ast(&ctx, lhs, lhs_kind)._eq(
                    &self.mir_binop_to_ast(
                        &ctx,
                        operator,
                        self.primval_to_ast(&ctx, rhs_operand1, kind),
                        self.primval_to_ast(&ctx, rhs_operand2, kind),
                        kind))
            }
            Constraint::Unop { operator, kind, lhs, operand, .. } => {
                self.primval_to_ast(&ctx, lhs, kind)._eq(
                    &self.mir_unop_to_ast(
                        &ctx,
                        operator,
                        self.primval_to_ast(&ctx, operand, kind)))
            }

            Constraint::NumericIntrinsic { operator, kind, lhs, operand, .. } => {
                self.primval_to_ast(&ctx, lhs, kind)._eq(
                    &self.mir_intrinsic_to_ast(
                        &ctx,
                        operator,
                        self.primval_to_ast(&ctx, operand, kind),
                        kind))
            }

            Constraint::Compare { op, lhs, rhs, kind, .. } => {
                // TODO(cleanup) this duplicates some functionality of mir_binop_to_ast().
                // Can we consolidate?
                match op {
                    mir::BinOp::Eq => {
                        self.primval_to_ast(&ctx, lhs, kind)._eq(
                            &self.primval_to_ast(&ctx, rhs, kind))
                    }
                    mir::BinOp::Ne => {
                        self.primval_to_ast(&ctx, lhs, kind)._eq(
                            &self.primval_to_ast(&ctx, rhs, kind)).not()
                    }
                    mir::BinOp::Gt => {
                        if kind.is_signed_int() {
                            unimplemented!()
                        }
                        self.primval_to_ast(&ctx, lhs, kind).bvugt(
                            &self.primval_to_ast(&ctx, rhs, kind))
                    }

                    mir::BinOp::Lt => {
                        if kind.is_signed_int() {
                            unimplemented!()
                        }
                        self.primval_to_ast(ctx, lhs, kind).bvult(
                            &self.primval_to_ast(&ctx, rhs, kind))
                    }

                    _ => {
                        unimplemented!()
                    }
                }
            }

            Constraint::IfThenElse { discriminant, kind, then_branch, else_branch, lhs } => {
                self.primval_to_ast(&ctx, lhs, kind)._eq(
                    &self.primval_to_ast(&ctx, discriminant, PrimValKind::Bool).ite(
                        &self.primval_to_ast(&ctx, then_branch, kind),
                        &self.primval_to_ast(&ctx, else_branch, kind)))

            }

            Constraint::ArrayElement { array, index, value, } => {
                let c = ::z3::Ast::new_const(
                    &::z3::Symbol::from_int(ctx, array.0),
                    &ctx.array_sort(
                        &ctx.bitvector_sort(64),
                        &ctx.bitvector_sort(8)));

                c.select(&self.primval_to_ast(ctx, index, PrimValKind::U64))._eq(
                    &self.sbyte_to_ast(ctx, value))
            }

            Constraint::ArrayStore { array, index, value, lhs } => {
                let c0 = ::z3::Ast::new_const(
                    &::z3::Symbol::from_int(ctx, array.0),
                    &ctx.array_sort(
                        &ctx.bitvector_sort(64),
                        &ctx.bitvector_sort(8)));

                let c1 = ::z3::Ast::new_const(
                    &::z3::Symbol::from_int(ctx, lhs.0),
                    &ctx.array_sort(
                        &ctx.bitvector_sort(64),
                        &ctx.bitvector_sort(8)));

                c1._eq(
                    &c0.store(
                        &self.primval_to_ast(ctx, index, PrimValKind::U64),
                        &self.sbyte_to_ast(ctx, value)))
            }

        }
    }

    fn primval_to_ast<'a>(
        &self,
        ctx: &'a z3::Context,
        primval: PrimVal,
        kind: PrimValKind)
        -> z3::Ast<'a>
    {
        match primval {
            PrimVal::Undef => {
                unimplemented!()
            }
            PrimVal::Ptr(_) => {
                unimplemented!()
            }
            PrimVal::Abstract(sbytes) => {
                if let PrimValKind::Bool = kind {
                    match sbytes[0] {
                        SByte::Abstract(b) => {
                            ctx.numbered_bool_const(b.0)
                        }
                        SByte::Concrete(_b) => {
                            unimplemented!()
                        }
                    }
                } else {
                    let num_bytes = kind.num_bytes();
                    self.sbyte_slice_to_ast(ctx, &sbytes[..num_bytes])
                }
            }
            PrimVal::Bytes(v) => {
                match kind {
                    PrimValKind::Bool => z3::Ast::from_bool(&ctx, v != 0),
                    PrimValKind::U8 | PrimValKind::I8 => z3::Ast::bv_from_u64(&ctx, v as u64, 8),
                    PrimValKind::U16 | PrimValKind::I16 => z3::Ast::bv_from_u64(&ctx, v as u64, 16),
                    PrimValKind::U32 | PrimValKind::I32 => z3::Ast::bv_from_u64(&ctx, v as u64, 32),
                    PrimValKind::U64 | PrimValKind::I64 => z3::Ast::bv_from_u64(&ctx, v as u64, 64),

                    PrimValKind::Char => z3::Ast::bv_from_u64(&ctx, v as u64, 32),

                    _ => {
                        unimplemented!()
                    }
                }
            }
        }
    }

    fn mir_binop_to_ast<'a>(
        &self,
        _ctx: &'a z3::Context,
        operator: mir::BinOp,
        left: z3::Ast<'a>,
        right: z3::Ast<'a>,
        kind: PrimValKind)
        -> z3::Ast<'a>
    {
        match (operator, kind) {
            (mir::BinOp::Eq, _) => left._eq(&right),
            (mir::BinOp::Ne, _) => left._eq(&right).not(),

            (mir::BinOp::Lt, kind) if kind.is_signed_int() => left.bvslt(&right),
            (mir::BinOp::Lt, _) => left.bvult(&right),

            (mir::BinOp::Le, kind) if kind.is_signed_int() => left.bvsle(&right),
            (mir::BinOp::Le, _) => left.bvule(&right),

            (mir::BinOp::Gt, kind) if kind.is_signed_int() => left.bvsgt(&right),
            (mir::BinOp::Gt, _) => left.bvugt(&right),

            (mir::BinOp::Ge, kind) if kind.is_signed_int() => left.bvsge(&right),
            (mir::BinOp::Ge, _) => left.bvuge(&right),

            (mir::BinOp::Add, _) => left.bvadd(&right),
            (mir::BinOp::Sub, _) => left.bvsub(&right),

            (mir::BinOp::BitXor, PrimValKind::Bool) => left.xor(&right),
            (mir::BinOp::BitXor, _) => left.bvxor(&right),

            (mir::BinOp::BitAnd, PrimValKind::Bool) => left.and(&[&right]),
            (mir::BinOp::BitAnd, _) => left.bvand(&right),

            (mir::BinOp::BitOr, PrimValKind::Bool) => left.or(&[&right]),
            (mir::BinOp::BitOr, _) => left.bvor(&right),

            (mir::BinOp::Mul, _) => left.bvmul(&right),
            (mir::BinOp::Shl, _) => left.bvshl(&right),

            (mir::BinOp::Shr, kind) if kind.is_signed_int() => left.bvashr(&right),
            (mir::BinOp::Shr, _) => left.bvlshr(&right),

            (mir::BinOp::Div, kind) if kind.is_signed_int() => left.bvsdiv(&right),
            (mir::BinOp::Div, _) => left.bvudiv(&right),

            (mir::BinOp::Rem, kind) if kind.is_signed_int() => left.bvsrem(&right),
            (mir::BinOp::Rem, _) => left.bvurem(&right),

            _ => {
                println!("{:?}", operator);
                unimplemented!()
            }
        }
    }

    fn mir_unop_to_ast<'a>(
        &self,
        _ctx: &'a z3::Context,
        operator: mir::UnOp,
        val: z3::Ast<'a>,)
        -> z3::Ast<'a>
    {
        match operator {
            mir::UnOp::Not => val.not(),
            mir::UnOp::Neg => val.bvneg(),
        }
    }

    fn mir_intrinsic_to_ast<'a>(
        &self,
        ctx: &'a z3::Context,
        operator: NumericIntrinsic,
        val: z3::Ast<'a>,
        kind: PrimValKind)
        -> z3::Ast<'a>
    {
        match operator {
            NumericIntrinsic::Ctpop => {
                // no magic in here, just mask each bit and sum them
                // this avoids branching (ite)
                let num_bits = kind.num_bytes() as u32 * 8;
                let zero = z3::Ast::bv_from_u64(&ctx, 0, num_bits);
                let one = z3::Ast::bv_from_u64(&ctx, 1, num_bits);
                (0..num_bits)
                    .map(|idx| z3::Ast::bv_from_u64(&ctx, idx as u64, num_bits))
                    .fold(zero, |r, idx| r.bvadd(&val.bvlshr(&idx).bvand(&one)))
            },
            NumericIntrinsic::Ctlz => {
                // from http://aggregate.org/MAGIC/#Leading%20Zero%20Count
                // ctlz(x) = bits - ones(z) where
                // z = x | x >> 1 | x >> 2 | x >> 3 | ... | x >> num_bits - 1
                let num_bits = kind.num_bytes() as u32 * 8;
                let zero = z3::Ast::bv_from_u64(&ctx, 0, num_bits);
                let z = (0..num_bits)
                    .map(|idx| z3::Ast::bv_from_u64(&ctx, idx as u64, num_bits))
                    .fold(zero, |x, idx| x.bvor(&val.bvlshr(&idx)));

                let ones = self.mir_intrinsic_to_ast(ctx, NumericIntrinsic::Ctpop, z, kind);
                z3::Ast::bv_from_u64(&ctx, num_bits as u64, num_bits).bvsub(&ones)
            },
            NumericIntrinsic::Cttz => {
                // from http://aggregate.org/MAGIC/#Trailing%20Zero%20Count
                // cttz(x) = ones((x & (−x)) − 1)
                let num_bits = kind.num_bytes() as u32 * 8;
                let one = z3::Ast::bv_from_u64(&ctx, 1, num_bits);
                let z = val.bvand(&val.bvneg()).bvsub(&one);
                self.mir_intrinsic_to_ast(ctx, NumericIntrinsic::Ctpop, z, kind)
            },
        }
    }
}
