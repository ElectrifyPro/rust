/// Type metadata identifiers (using Itanium C++ ABI mangling for encoding) for LLVM Control Flow
/// Integrity (CFI) and cross-language LLVM CFI support.
///
/// Encodes type metadata identifiers for LLVM CFI and cross-language LLVM CFI support using Itanium
/// C++ ABI mangling for encoding with vendor extended type qualifiers and types for Rust types that
/// are not used across the FFI boundary.
///
/// For more information about LLVM CFI and cross-language LLVM CFI support for the Rust compiler,
/// see design document in the tracking issue #89653.
use rustc_data_structures::base_n;
use rustc_data_structures::fx::FxHashMap;
use rustc_hir as hir;
use rustc_hir::lang_items::LangItem;
use rustc_middle::ty::fold::{TypeFolder, TypeSuperFoldable};
use rustc_middle::ty::layout::IntegerExt;
use rustc_middle::ty::{
    self, Const, ExistentialPredicate, FloatTy, FnSig, Instance, IntTy, List, Region, RegionKind,
    TermKind, Ty, TyCtxt, UintTy,
};
use rustc_middle::ty::{GenericArg, GenericArgKind, GenericArgsRef};
use rustc_middle::ty::{TypeFoldable, TypeVisitableExt};
use rustc_span::def_id::DefId;
use rustc_span::sym;
use rustc_target::abi::call::{Conv, FnAbi, PassMode};
use rustc_target::abi::Integer;
use rustc_target::spec::abi::Abi;
use rustc_trait_selection::traits;
use std::fmt::Write as _;
use std::iter;

use crate::typeid::TypeIdOptions;

/// Type and extended type qualifiers.
#[derive(Eq, Hash, PartialEq)]
enum TyQ {
    None,
    Const,
    Mut,
}

/// Substitution dictionary key.
#[derive(Eq, Hash, PartialEq)]
enum DictKey<'tcx> {
    Ty(Ty<'tcx>, TyQ),
    Region(Region<'tcx>),
    Const(Const<'tcx>),
    Predicate(ExistentialPredicate<'tcx>),
}

/// Options for encode_ty.
type EncodeTyOptions = TypeIdOptions;

/// Options for transform_ty.
type TransformTyOptions = TypeIdOptions;

/// Converts a number to a disambiguator (see
/// <https://rust-lang.github.io/rfcs/2603-rust-symbol-name-mangling-v0.html>).
fn to_disambiguator(num: u64) -> String {
    if let Some(num) = num.checked_sub(1) {
        format!("s{}_", base_n::encode(num as u128, 62))
    } else {
        "s_".to_string()
    }
}

/// Converts a number to a sequence number (see
/// <https://itanium-cxx-abi.github.io/cxx-abi/abi.html#mangle.seq-id>).
fn to_seq_id(num: usize) -> String {
    if let Some(num) = num.checked_sub(1) {
        base_n::encode(num as u128, 36).to_uppercase()
    } else {
        "".to_string()
    }
}

/// Substitutes a component if found in the substitution dictionary (see
/// <https://itanium-cxx-abi.github.io/cxx-abi/abi.html#mangling-compression>).
fn compress<'tcx>(
    dict: &mut FxHashMap<DictKey<'tcx>, usize>,
    key: DictKey<'tcx>,
    comp: &mut String,
) {
    match dict.get(&key) {
        Some(num) => {
            comp.clear();
            let _ = write!(comp, "S{}_", to_seq_id(*num));
        }
        None => {
            dict.insert(key, dict.len());
        }
    }
}

/// Encodes a const using the Itanium C++ ABI as a literal argument (see
/// <https://itanium-cxx-abi.github.io/cxx-abi/abi.html#mangling.literal>).
fn encode_const<'tcx>(
    tcx: TyCtxt<'tcx>,
    c: Const<'tcx>,
    dict: &mut FxHashMap<DictKey<'tcx>, usize>,
    options: EncodeTyOptions,
) -> String {
    // L<element-type>[n][<element-value>]E as literal argument
    let mut s = String::from('L');

    match c.kind() {
        // Const parameters
        ty::ConstKind::Param(..) => {
            // L<element-type>E as literal argument

            // Element type
            s.push_str(&encode_ty(tcx, c.ty(), dict, options));
        }

        // Literal arguments
        ty::ConstKind::Value(..) => {
            // L<element-type>[n]<element-value>E as literal argument

            // Element type
            s.push_str(&encode_ty(tcx, c.ty(), dict, options));

            // The only allowed types of const values are bool, u8, u16, u32,
            // u64, u128, usize i8, i16, i32, i64, i128, isize, and char. The
            // bool value false is encoded as 0 and true as 1.
            match c.ty().kind() {
                ty::Int(ity) => {
                    let bits = c.eval_bits(tcx, ty::ParamEnv::reveal_all());
                    let val = Integer::from_int_ty(&tcx, *ity).size().sign_extend(bits) as i128;
                    if val < 0 {
                        s.push('n');
                    }
                    let _ = write!(s, "{val}");
                }
                ty::Uint(_) => {
                    let val = c.eval_bits(tcx, ty::ParamEnv::reveal_all());
                    let _ = write!(s, "{val}");
                }
                ty::Bool => {
                    let val = c.try_eval_bool(tcx, ty::ParamEnv::reveal_all()).unwrap();
                    let _ = write!(s, "{val}");
                }
                _ => {
                    bug!("encode_const: unexpected type `{:?}`", c.ty());
                }
            }
        }

        _ => {
            bug!("encode_const: unexpected kind `{:?}`", c.kind());
        }
    }

    // Close the "L..E" pair
    s.push('E');

    compress(dict, DictKey::Const(c), &mut s);

    s
}

/// Encodes a FnSig using the Itanium C++ ABI with vendor extended type qualifiers and types for
/// Rust types that are not used at the FFI boundary.
#[instrument(level = "trace", skip(tcx, dict))]
fn encode_fnsig<'tcx>(
    tcx: TyCtxt<'tcx>,
    fn_sig: &FnSig<'tcx>,
    dict: &mut FxHashMap<DictKey<'tcx>, usize>,
    options: TypeIdOptions,
) -> String {
    // Function types are delimited by an "F..E" pair
    let mut s = String::from("F");

    let mut encode_ty_options = EncodeTyOptions::from_bits(options.bits())
        .unwrap_or_else(|| bug!("encode_fnsig: invalid option(s) `{:?}`", options.bits()));
    match fn_sig.abi {
        Abi::C { .. } => {
            encode_ty_options.insert(EncodeTyOptions::GENERALIZE_REPR_C);
        }
        _ => {
            encode_ty_options.remove(EncodeTyOptions::GENERALIZE_REPR_C);
        }
    }

    // Encode the return type
    let transform_ty_options = TransformTyOptions::from_bits(options.bits())
        .unwrap_or_else(|| bug!("encode_fnsig: invalid option(s) `{:?}`", options.bits()));
    let mut type_folder = TransformTy::new(tcx, transform_ty_options);
    let ty = fn_sig.output().fold_with(&mut type_folder);
    s.push_str(&encode_ty(tcx, ty, dict, encode_ty_options));

    // Encode the parameter types
    let tys = fn_sig.inputs();
    if !tys.is_empty() {
        for ty in tys {
            let ty = ty.fold_with(&mut type_folder);
            s.push_str(&encode_ty(tcx, ty, dict, encode_ty_options));
        }

        if fn_sig.c_variadic {
            s.push('z');
        }
    } else {
        if fn_sig.c_variadic {
            s.push('z');
        } else {
            // Empty parameter lists, whether declared as () or conventionally as (void), are
            // encoded with a void parameter specifier "v".
            s.push('v')
        }
    }

    // Close the "F..E" pair
    s.push('E');

    s
}

/// Encodes a predicate using the Itanium C++ ABI with vendor extended type qualifiers and types for
/// Rust types that are not used at the FFI boundary.
fn encode_predicate<'tcx>(
    tcx: TyCtxt<'tcx>,
    predicate: ty::PolyExistentialPredicate<'tcx>,
    dict: &mut FxHashMap<DictKey<'tcx>, usize>,
    options: EncodeTyOptions,
) -> String {
    // u<length><name>[I<element-type1..element-typeN>E], where <element-type> is <subst>, as vendor
    // extended type.
    let mut s = String::new();
    match predicate.as_ref().skip_binder() {
        ty::ExistentialPredicate::Trait(trait_ref) => {
            let name = encode_ty_name(tcx, trait_ref.def_id);
            let _ = write!(s, "u{}{}", name.len(), &name);
            s.push_str(&encode_args(tcx, trait_ref.args, dict, options));
        }
        ty::ExistentialPredicate::Projection(projection) => {
            let name = encode_ty_name(tcx, projection.def_id);
            let _ = write!(s, "u{}{}", name.len(), &name);
            s.push_str(&encode_args(tcx, projection.args, dict, options));
            match projection.term.unpack() {
                TermKind::Ty(ty) => s.push_str(&encode_ty(tcx, ty, dict, options)),
                TermKind::Const(c) => s.push_str(&encode_const(tcx, c, dict, options)),
            }
        }
        ty::ExistentialPredicate::AutoTrait(def_id) => {
            let name = encode_ty_name(tcx, *def_id);
            let _ = write!(s, "u{}{}", name.len(), &name);
        }
    };
    compress(dict, DictKey::Predicate(*predicate.as_ref().skip_binder()), &mut s);
    s
}

/// Encodes predicates using the Itanium C++ ABI with vendor extended type qualifiers and types for
/// Rust types that are not used at the FFI boundary.
fn encode_predicates<'tcx>(
    tcx: TyCtxt<'tcx>,
    predicates: &List<ty::PolyExistentialPredicate<'tcx>>,
    dict: &mut FxHashMap<DictKey<'tcx>, usize>,
    options: EncodeTyOptions,
) -> String {
    // <predicate1[..predicateN]>E as part of vendor extended type
    let mut s = String::new();
    let predicates: Vec<ty::PolyExistentialPredicate<'tcx>> = predicates.iter().collect();
    for predicate in predicates {
        s.push_str(&encode_predicate(tcx, predicate, dict, options));
    }
    s
}

/// Encodes a region using the Itanium C++ ABI as a vendor extended type.
fn encode_region<'tcx>(region: Region<'tcx>, dict: &mut FxHashMap<DictKey<'tcx>, usize>) -> String {
    // u6region[I[<region-disambiguator>][<region-index>]E] as vendor extended type
    let mut s = String::new();
    match region.kind() {
        RegionKind::ReBound(debruijn, r) => {
            s.push_str("u6regionI");
            // Debruijn index, which identifies the binder, as region disambiguator
            let num = debruijn.index() as u64;
            if num > 0 {
                s.push_str(&to_disambiguator(num));
            }
            // Index within the binder
            let _ = write!(s, "{}", r.var.index() as u64);
            s.push('E');
            compress(dict, DictKey::Region(region), &mut s);
        }
        RegionKind::ReErased => {
            s.push_str("u6region");
            compress(dict, DictKey::Region(region), &mut s);
        }
        RegionKind::ReEarlyParam(..)
        | RegionKind::ReLateParam(..)
        | RegionKind::ReStatic
        | RegionKind::ReError(_)
        | RegionKind::ReVar(..)
        | RegionKind::RePlaceholder(..) => {
            bug!("encode_region: unexpected `{:?}`", region.kind());
        }
    }
    s
}

/// Encodes args using the Itanium C++ ABI with vendor extended type qualifiers and types for Rust
/// types that are not used at the FFI boundary.
fn encode_args<'tcx>(
    tcx: TyCtxt<'tcx>,
    args: GenericArgsRef<'tcx>,
    dict: &mut FxHashMap<DictKey<'tcx>, usize>,
    options: EncodeTyOptions,
) -> String {
    // [I<subst1..substN>E] as part of vendor extended type
    let mut s = String::new();
    let args: Vec<GenericArg<'_>> = args.iter().collect();
    if !args.is_empty() {
        s.push('I');
        for arg in args {
            match arg.unpack() {
                GenericArgKind::Lifetime(region) => {
                    s.push_str(&encode_region(region, dict));
                }
                GenericArgKind::Type(ty) => {
                    s.push_str(&encode_ty(tcx, ty, dict, options));
                }
                GenericArgKind::Const(c) => {
                    s.push_str(&encode_const(tcx, c, dict, options));
                }
            }
        }
        s.push('E');
    }
    s
}

/// Encodes a ty:Ty name, including its crate and path disambiguators and names.
fn encode_ty_name(tcx: TyCtxt<'_>, def_id: DefId) -> String {
    // Encode <name> for use in u<length><name>[I<element-type1..element-typeN>E], where
    // <element-type> is <subst>, using v0's <path> without v0's extended form of paths:
    //
    // N<namespace-tagN>..N<namespace-tag1>
    // C<crate-disambiguator><crate-name>
    // <path-disambiguator1><path-name1>..<path-disambiguatorN><path-nameN>
    //
    // With additional tags for DefPathData::Impl and DefPathData::ForeignMod. For instance:
    //
    //     pub type Type1 = impl Send;
    //     let _: Type1 = <Struct1<i32>>::foo;
    //     fn foo1(_: Type1) { }
    //
    //     pub type Type2 = impl Send;
    //     let _: Type2 = <Trait1<i32>>::foo;
    //     fn foo2(_: Type2) { }
    //
    //     pub type Type3 = impl Send;
    //     let _: Type3 = <i32 as Trait1<i32>>::foo;
    //     fn foo3(_: Type3) { }
    //
    //     pub type Type4 = impl Send;
    //     let _: Type4 = <Struct1<i32> as Trait1<i32>>::foo;
    //     fn foo3(_: Type4) { }
    //
    // Are encoded as:
    //
    //     _ZTSFvu29NvNIC1234_5crate8{{impl}}3fooIu3i32EE
    //     _ZTSFvu27NvNtC1234_5crate6Trait13fooIu3dynIu21NtC1234_5crate6Trait1Iu3i32Eu6regionES_EE
    //     _ZTSFvu27NvNtC1234_5crate6Trait13fooIu3i32S_EE
    //     _ZTSFvu27NvNtC1234_5crate6Trait13fooIu22NtC1234_5crate7Struct1Iu3i32ES_EE
    //
    // The reason for not using v0's extended form of paths is to use a consistent and simpler
    // encoding, as the reasoning for using it isn't relevant for type metadata identifiers (i.e.,
    // keep symbol names close to how methods are represented in error messages). See
    // https://rust-lang.github.io/rfcs/2603-rust-symbol-name-mangling-v0.html#methods.
    let mut s = String::new();

    // Start and namespace tags
    let mut def_path = tcx.def_path(def_id);
    def_path.data.reverse();
    for disambiguated_data in &def_path.data {
        s.push('N');
        s.push_str(match disambiguated_data.data {
            hir::definitions::DefPathData::Impl => "I", // Not specified in v0's <namespace>
            hir::definitions::DefPathData::ForeignMod => "F", // Not specified in v0's <namespace>
            hir::definitions::DefPathData::TypeNs(..) => "t",
            hir::definitions::DefPathData::ValueNs(..) => "v",
            hir::definitions::DefPathData::Closure => "C",
            hir::definitions::DefPathData::Ctor => "c",
            hir::definitions::DefPathData::AnonConst => "k",
            hir::definitions::DefPathData::OpaqueTy => "i",
            hir::definitions::DefPathData::CrateRoot
            | hir::definitions::DefPathData::Use
            | hir::definitions::DefPathData::GlobalAsm
            | hir::definitions::DefPathData::MacroNs(..)
            | hir::definitions::DefPathData::LifetimeNs(..)
            | hir::definitions::DefPathData::AnonAdt => {
                bug!("encode_ty_name: unexpected `{:?}`", disambiguated_data.data);
            }
        });
    }

    // Crate disambiguator and name
    s.push('C');
    s.push_str(&to_disambiguator(tcx.stable_crate_id(def_path.krate).as_u64()));
    let crate_name = tcx.crate_name(def_path.krate).to_string();
    let _ = write!(s, "{}{}", crate_name.len(), &crate_name);

    // Disambiguators and names
    def_path.data.reverse();
    for disambiguated_data in &def_path.data {
        let num = disambiguated_data.disambiguator as u64;
        if num > 0 {
            s.push_str(&to_disambiguator(num));
        }

        let name = disambiguated_data.data.to_string();
        let _ = write!(s, "{}", name.len());

        // Prepend a '_' if name starts with a digit or '_'
        if let Some(first) = name.as_bytes().first() {
            if first.is_ascii_digit() || *first == b'_' {
                s.push('_');
            }
        } else {
            bug!("encode_ty_name: invalid name `{:?}`", name);
        }

        s.push_str(&name);
    }

    s
}

/// Encodes a ty:Ty using the Itanium C++ ABI with vendor extended type qualifiers and types for
/// Rust types that are not used at the FFI boundary.
fn encode_ty<'tcx>(
    tcx: TyCtxt<'tcx>,
    ty: Ty<'tcx>,
    dict: &mut FxHashMap<DictKey<'tcx>, usize>,
    options: EncodeTyOptions,
) -> String {
    let mut typeid = String::new();

    match ty.kind() {
        // Primitive types

        // Rust's bool has the same layout as C17's _Bool, that is, its size and alignment are
        // implementation-defined. Any bool can be cast into an integer, taking on the values 1
        // (true) or 0 (false).
        //
        // (See https://rust-lang.github.io/unsafe-code-guidelines/layout/scalars.html#bool.)
        ty::Bool => {
            typeid.push('b');
        }

        ty::Int(..) | ty::Uint(..) => {
            // u<length><type-name> as vendor extended type
            let mut s = String::from(match ty.kind() {
                ty::Int(IntTy::I8) => "u2i8",
                ty::Int(IntTy::I16) => "u3i16",
                ty::Int(IntTy::I32) => "u3i32",
                ty::Int(IntTy::I64) => "u3i64",
                ty::Int(IntTy::I128) => "u4i128",
                ty::Int(IntTy::Isize) => "u5isize",
                ty::Uint(UintTy::U8) => "u2u8",
                ty::Uint(UintTy::U16) => "u3u16",
                ty::Uint(UintTy::U32) => "u3u32",
                ty::Uint(UintTy::U64) => "u3u64",
                ty::Uint(UintTy::U128) => "u4u128",
                ty::Uint(UintTy::Usize) => "u5usize",
                _ => bug!("encode_ty: unexpected `{:?}`", ty.kind()),
            });
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        // Rust's f16, f32, f64, and f126 half (16-bit), single (32-bit), double (64-bit), and
        // quad (128-bit)  precision floating-point types have IEEE-754 binary16, binary32,
        // binary64, and binary128 floating-point layouts, respectively.
        //
        // (See https://rust-lang.github.io/unsafe-code-guidelines/layout/scalars.html#fixed-width-floating-point-types.)
        ty::Float(float_ty) => {
            typeid.push_str(match float_ty {
                FloatTy::F16 => "Dh",
                FloatTy::F32 => "f",
                FloatTy::F64 => "d",
                FloatTy::F128 => "g",
            });
        }

        ty::Char => {
            // u4char as vendor extended type
            let mut s = String::from("u4char");
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        ty::Str => {
            // u3str as vendor extended type
            let mut s = String::from("u3str");
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        ty::Never => {
            // u5never as vendor extended type
            let mut s = String::from("u5never");
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        // Compound types
        // () in Rust is equivalent to void return type in C
        _ if ty.is_unit() => {
            typeid.push('v');
        }

        // Sequence types
        ty::Tuple(tys) => {
            // u5tupleI<element-type1..element-typeN>E as vendor extended type
            let mut s = String::from("u5tupleI");
            for ty in tys.iter() {
                s.push_str(&encode_ty(tcx, ty, dict, options));
            }
            s.push('E');
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        ty::Array(ty0, len) => {
            // A<array-length><element-type>
            let len = len.eval_target_usize(tcx, ty::ParamEnv::reveal_all());
            let mut s = String::from("A");
            let _ = write!(s, "{}", &len);
            s.push_str(&encode_ty(tcx, *ty0, dict, options));
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        ty::Pat(ty0, pat) => {
            // u3patI<element-type><pattern>E as vendor extended type
            let mut s = String::from("u3patI");
            s.push_str(&encode_ty(tcx, *ty0, dict, options));
            write!(s, "{:?}", **pat).unwrap();
            s.push('E');
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        ty::Slice(ty0) => {
            // u5sliceI<element-type>E as vendor extended type
            let mut s = String::from("u5sliceI");
            s.push_str(&encode_ty(tcx, *ty0, dict, options));
            s.push('E');
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        // User-defined types
        ty::Adt(adt_def, args) => {
            let mut s = String::new();
            let def_id = adt_def.did();
            if let Some(cfi_encoding) = tcx.get_attr(def_id, sym::cfi_encoding) {
                // Use user-defined CFI encoding for type
                if let Some(value_str) = cfi_encoding.value_str() {
                    let value_str = value_str.to_string();
                    let str = value_str.trim();
                    if !str.is_empty() {
                        s.push_str(str);
                        // Don't compress user-defined builtin types (see
                        // https://itanium-cxx-abi.github.io/cxx-abi/abi.html#mangling-builtin and
                        // https://itanium-cxx-abi.github.io/cxx-abi/abi.html#mangling-compression).
                        let builtin_types = [
                            "v", "w", "b", "c", "a", "h", "s", "t", "i", "j", "l", "m", "x", "y",
                            "n", "o", "f", "d", "e", "g", "z", "Dh",
                        ];
                        if !builtin_types.contains(&str) {
                            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
                        }
                    } else {
                        #[allow(
                            rustc::diagnostic_outside_of_impl,
                            rustc::untranslatable_diagnostic
                        )]
                        tcx.dcx()
                            .struct_span_err(
                                cfi_encoding.span,
                                format!("invalid `cfi_encoding` for `{:?}`", ty.kind()),
                            )
                            .emit();
                    }
                } else {
                    bug!("encode_ty: invalid `cfi_encoding` for `{:?}`", ty.kind());
                }
            } else if options.contains(EncodeTyOptions::GENERALIZE_REPR_C) && adt_def.repr().c() {
                // For cross-language LLVM CFI support, the encoding must be compatible at the FFI
                // boundary. For instance:
                //
                //     struct type1 {};
                //     void foo(struct type1* bar) {}
                //
                // Is encoded as:
                //
                //     _ZTSFvP5type1E
                //
                // So, encode any repr(C) user-defined type for extern function types with the "C"
                // calling convention (or extern types [i.e., ty::Foreign]) as <length><name>, where
                // <name> is <unscoped-name>.
                let name = tcx.item_name(def_id).to_string();
                let _ = write!(s, "{}{}", name.len(), &name);
                compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            } else {
                // u<length><name>[I<element-type1..element-typeN>E], where <element-type> is
                // <subst>, as vendor extended type.
                let name = encode_ty_name(tcx, def_id);
                let _ = write!(s, "u{}{}", name.len(), &name);
                s.push_str(&encode_args(tcx, args, dict, options));
                compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            }
            typeid.push_str(&s);
        }

        ty::Foreign(def_id) => {
            // <length><name>, where <name> is <unscoped-name>
            let mut s = String::new();
            if let Some(cfi_encoding) = tcx.get_attr(*def_id, sym::cfi_encoding) {
                // Use user-defined CFI encoding for type
                if let Some(value_str) = cfi_encoding.value_str() {
                    if !value_str.to_string().trim().is_empty() {
                        s.push_str(value_str.to_string().trim());
                    } else {
                        #[allow(
                            rustc::diagnostic_outside_of_impl,
                            rustc::untranslatable_diagnostic
                        )]
                        tcx.dcx()
                            .struct_span_err(
                                cfi_encoding.span,
                                format!("invalid `cfi_encoding` for `{:?}`", ty.kind()),
                            )
                            .emit();
                    }
                } else {
                    bug!("encode_ty: invalid `cfi_encoding` for `{:?}`", ty.kind());
                }
            } else {
                let name = tcx.item_name(*def_id).to_string();
                let _ = write!(s, "{}{}", name.len(), &name);
            }
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        // Function types
        ty::FnDef(def_id, args) | ty::Closure(def_id, args) => {
            // u<length><name>[I<element-type1..element-typeN>E], where <element-type> is <subst>,
            // as vendor extended type.
            let mut s = String::new();
            let name = encode_ty_name(tcx, *def_id);
            let _ = write!(s, "u{}{}", name.len(), &name);
            s.push_str(&encode_args(tcx, args, dict, options));
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        ty::CoroutineClosure(def_id, args) => {
            // u<length><name>[I<element-type1..element-typeN>E], where <element-type> is <subst>,
            // as vendor extended type.
            let mut s = String::new();
            let name = encode_ty_name(tcx, *def_id);
            let _ = write!(s, "u{}{}", name.len(), &name);
            let parent_args = tcx.mk_args(args.as_coroutine_closure().parent_args());
            s.push_str(&encode_args(tcx, parent_args, dict, options));
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        ty::Coroutine(def_id, args, ..) => {
            // u<length><name>[I<element-type1..element-typeN>E], where <element-type> is <subst>,
            // as vendor extended type.
            let mut s = String::new();
            let name = encode_ty_name(tcx, *def_id);
            let _ = write!(s, "u{}{}", name.len(), &name);
            // Encode parent args only
            s.push_str(&encode_args(
                tcx,
                tcx.mk_args(args.as_coroutine().parent_args()),
                dict,
                options,
            ));
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        // Pointer types
        ty::Ref(region, ty0, ..) => {
            // [U3mut]u3refI<element-type>E as vendor extended type qualifier and type
            let mut s = String::new();
            s.push_str("u3refI");
            s.push_str(&encode_ty(tcx, *ty0, dict, options));
            s.push('E');
            compress(dict, DictKey::Ty(Ty::new_imm_ref(tcx, *region, *ty0), TyQ::None), &mut s);
            if ty.is_mutable_ptr() {
                s = format!("{}{}", "U3mut", &s);
                compress(dict, DictKey::Ty(ty, TyQ::Mut), &mut s);
            }
            typeid.push_str(&s);
        }

        ty::RawPtr(ptr_ty, _mutbl) => {
            // FIXME: This can definitely not be so spaghettified.
            // P[K]<element-type>
            let mut s = String::new();
            s.push_str(&encode_ty(tcx, *ptr_ty, dict, options));
            if !ty.is_mutable_ptr() {
                s = format!("{}{}", "K", &s);
                compress(dict, DictKey::Ty(*ptr_ty, TyQ::Const), &mut s);
            };
            s = format!("{}{}", "P", &s);
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        ty::FnPtr(fn_sig) => {
            // PF<return-type><parameter-type1..parameter-typeN>E
            let mut s = String::from("P");
            s.push_str(&encode_fnsig(tcx, &fn_sig.skip_binder(), dict, TypeIdOptions::empty()));
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        // Trait types
        ty::Dynamic(predicates, region, kind) => {
            // u3dynI<element-type1[..element-typeN]>E, where <element-type> is <predicate>, as
            // vendor extended type.
            let mut s = String::from(match kind {
                ty::Dyn => "u3dynI",
                ty::DynStar => "u7dynstarI",
            });
            s.push_str(&encode_predicates(tcx, predicates, dict, options));
            s.push_str(&encode_region(*region, dict));
            s.push('E');
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        // Type parameters
        ty::Param(..) => {
            // u5param as vendor extended type
            let mut s = String::from("u5param");
            compress(dict, DictKey::Ty(ty, TyQ::None), &mut s);
            typeid.push_str(&s);
        }

        // Unexpected types
        ty::Alias(..)
        | ty::Bound(..)
        | ty::Error(..)
        | ty::CoroutineWitness(..)
        | ty::Infer(..)
        | ty::Placeholder(..) => {
            bug!("encode_ty: unexpected `{:?}`", ty.kind());
        }
    };

    typeid
}

struct TransformTy<'tcx> {
    tcx: TyCtxt<'tcx>,
    options: TransformTyOptions,
    parents: Vec<Ty<'tcx>>,
}

impl<'tcx> TransformTy<'tcx> {
    fn new(tcx: TyCtxt<'tcx>, options: TransformTyOptions) -> Self {
        TransformTy { tcx, options, parents: Vec::new() }
    }
}

impl<'tcx> TypeFolder<TyCtxt<'tcx>> for TransformTy<'tcx> {
    // Transforms a ty:Ty for being encoded and used in the substitution dictionary. It transforms
    // all c_void types into unit types unconditionally, generalizes pointers if
    // TransformTyOptions::GENERALIZE_POINTERS option is set, and normalizes integers if
    // TransformTyOptions::NORMALIZE_INTEGERS option is set.
    fn fold_ty(&mut self, t: Ty<'tcx>) -> Ty<'tcx> {
        match t.kind() {
            ty::Array(..)
            | ty::Closure(..)
            | ty::Coroutine(..)
            | ty::CoroutineClosure(..)
            | ty::CoroutineWitness(..)
            | ty::Dynamic(..)
            | ty::Float(..)
            | ty::FnDef(..)
            | ty::Foreign(..)
            | ty::Never
            | ty::Slice(..)
            | ty::Pat(..)
            | ty::Str
            | ty::Tuple(..) => t.super_fold_with(self),

            ty::Bool => {
                if self.options.contains(EncodeTyOptions::NORMALIZE_INTEGERS) {
                    // Note: on all platforms that Rust's currently supports, its size and alignment
                    // are 1, and its ABI class is INTEGER - see Rust Layout and ABIs.
                    //
                    // (See https://rust-lang.github.io/unsafe-code-guidelines/layout/scalars.html#bool.)
                    //
                    // Clang represents bool as an 8-bit unsigned integer.
                    self.tcx.types.u8
                } else {
                    t
                }
            }

            ty::Char => {
                if self.options.contains(EncodeTyOptions::NORMALIZE_INTEGERS) {
                    // Since #118032, char is guaranteed to have the same size, alignment, and
                    // function call ABI as u32 on all platforms.
                    self.tcx.types.u32
                } else {
                    t
                }
            }

            ty::Int(..) | ty::Uint(..) => {
                if self.options.contains(EncodeTyOptions::NORMALIZE_INTEGERS) {
                    // Note: C99 7.18.2.4 requires uintptr_t and intptr_t to be at least 16-bit
                    // wide. All platforms we currently support have a C platform, and as a
                    // consequence, isize/usize are at least 16-bit wide for all of them.
                    //
                    // (See https://rust-lang.github.io/unsafe-code-guidelines/layout/scalars.html#isize-and-usize.)
                    match t.kind() {
                        ty::Int(IntTy::Isize) => match self.tcx.sess.target.pointer_width {
                            16 => self.tcx.types.i16,
                            32 => self.tcx.types.i32,
                            64 => self.tcx.types.i64,
                            128 => self.tcx.types.i128,
                            _ => bug!(
                                "fold_ty: unexpected pointer width `{}`",
                                self.tcx.sess.target.pointer_width
                            ),
                        },
                        ty::Uint(UintTy::Usize) => match self.tcx.sess.target.pointer_width {
                            16 => self.tcx.types.u16,
                            32 => self.tcx.types.u32,
                            64 => self.tcx.types.u64,
                            128 => self.tcx.types.u128,
                            _ => bug!(
                                "fold_ty: unexpected pointer width `{}`",
                                self.tcx.sess.target.pointer_width
                            ),
                        },
                        _ => t,
                    }
                } else {
                    t
                }
            }

            ty::Adt(..) if t.is_c_void(self.tcx) => self.tcx.types.unit,

            ty::Adt(adt_def, args) => {
                if adt_def.repr().transparent() && adt_def.is_struct() && !self.parents.contains(&t)
                {
                    // Don't transform repr(transparent) types with an user-defined CFI encoding to
                    // preserve the user-defined CFI encoding.
                    if let Some(_) = self.tcx.get_attr(adt_def.did(), sym::cfi_encoding) {
                        return t;
                    }
                    let variant = adt_def.non_enum_variant();
                    let param_env = self.tcx.param_env(variant.def_id);
                    let field = variant.fields.iter().find(|field| {
                        let ty = self.tcx.type_of(field.did).instantiate_identity();
                        let is_zst = self
                            .tcx
                            .layout_of(param_env.and(ty))
                            .is_ok_and(|layout| layout.is_zst());
                        !is_zst
                    });
                    if let Some(field) = field {
                        let ty0 = self.tcx.type_of(field.did).instantiate(self.tcx, args);
                        // Generalize any repr(transparent) user-defined type that is either a
                        // pointer or reference, and either references itself or any other type that
                        // contains or references itself, to avoid a reference cycle.

                        // If the self reference is not through a pointer, for example, due
                        // to using `PhantomData`, need to skip normalizing it if we hit it again.
                        self.parents.push(t);
                        let ty = if ty0.is_any_ptr() && ty0.contains(t) {
                            let options = self.options;
                            self.options |= TransformTyOptions::GENERALIZE_POINTERS;
                            let ty = ty0.fold_with(self);
                            self.options = options;
                            ty
                        } else {
                            ty0.fold_with(self)
                        };
                        self.parents.pop();
                        ty
                    } else {
                        // Transform repr(transparent) types without non-ZST field into ()
                        self.tcx.types.unit
                    }
                } else {
                    t.super_fold_with(self)
                }
            }

            ty::Ref(..) => {
                if self.options.contains(TransformTyOptions::GENERALIZE_POINTERS) {
                    if t.is_mutable_ptr() {
                        Ty::new_mut_ref(self.tcx, self.tcx.lifetimes.re_static, self.tcx.types.unit)
                    } else {
                        Ty::new_imm_ref(self.tcx, self.tcx.lifetimes.re_static, self.tcx.types.unit)
                    }
                } else {
                    t.super_fold_with(self)
                }
            }

            ty::RawPtr(..) => {
                if self.options.contains(TransformTyOptions::GENERALIZE_POINTERS) {
                    if t.is_mutable_ptr() {
                        Ty::new_mut_ptr(self.tcx, self.tcx.types.unit)
                    } else {
                        Ty::new_imm_ptr(self.tcx, self.tcx.types.unit)
                    }
                } else {
                    t.super_fold_with(self)
                }
            }

            ty::FnPtr(..) => {
                if self.options.contains(TransformTyOptions::GENERALIZE_POINTERS) {
                    Ty::new_imm_ptr(self.tcx, self.tcx.types.unit)
                } else {
                    t.super_fold_with(self)
                }
            }

            ty::Alias(..) => {
                self.fold_ty(self.tcx.normalize_erasing_regions(ty::ParamEnv::reveal_all(), t))
            }

            ty::Bound(..) | ty::Error(..) | ty::Infer(..) | ty::Param(..) | ty::Placeholder(..) => {
                bug!("fold_ty: unexpected `{:?}`", t.kind());
            }
        }
    }

    fn interner(&self) -> TyCtxt<'tcx> {
        self.tcx
    }
}

/// Returns a type metadata identifier for the specified FnAbi using the Itanium C++ ABI with vendor
/// extended type qualifiers and types for Rust types that are not used at the FFI boundary.
#[instrument(level = "trace", skip(tcx))]
pub fn typeid_for_fnabi<'tcx>(
    tcx: TyCtxt<'tcx>,
    fn_abi: &FnAbi<'tcx, Ty<'tcx>>,
    options: TypeIdOptions,
) -> String {
    // A name is mangled by prefixing "_Z" to an encoding of its name, and in the case of functions
    // its type.
    let mut typeid = String::from("_Z");

    // Clang uses the Itanium C++ ABI's virtual tables and RTTI typeinfo structure name as type
    // metadata identifiers for function pointers. The typeinfo name encoding is a two-character
    // code (i.e., 'TS') prefixed to the type encoding for the function.
    typeid.push_str("TS");

    // Function types are delimited by an "F..E" pair
    typeid.push('F');

    // A dictionary of substitution candidates used for compression (see
    // https://itanium-cxx-abi.github.io/cxx-abi/abi.html#mangling-compression).
    let mut dict: FxHashMap<DictKey<'tcx>, usize> = FxHashMap::default();

    let mut encode_ty_options = EncodeTyOptions::from_bits(options.bits())
        .unwrap_or_else(|| bug!("typeid_for_fnabi: invalid option(s) `{:?}`", options.bits()));
    match fn_abi.conv {
        Conv::C => {
            encode_ty_options.insert(EncodeTyOptions::GENERALIZE_REPR_C);
        }
        _ => {
            encode_ty_options.remove(EncodeTyOptions::GENERALIZE_REPR_C);
        }
    }

    // Encode the return type
    let transform_ty_options = TransformTyOptions::from_bits(options.bits())
        .unwrap_or_else(|| bug!("typeid_for_fnabi: invalid option(s) `{:?}`", options.bits()));
    let mut type_folder = TransformTy::new(tcx, transform_ty_options);
    let ty = fn_abi.ret.layout.ty.fold_with(&mut type_folder);
    typeid.push_str(&encode_ty(tcx, ty, &mut dict, encode_ty_options));

    // Encode the parameter types

    // We erase ZSTs as we go if the argument is skipped. This is an implementation detail of how
    // MIR is currently treated by rustc, and subject to change in the future. Specifically, MIR
    // interpretation today will allow skipped arguments to simply not be passed at a call-site.
    if !fn_abi.c_variadic {
        let mut pushed_arg = false;
        for arg in fn_abi.args.iter().filter(|arg| arg.mode != PassMode::Ignore) {
            pushed_arg = true;
            let ty = arg.layout.ty.fold_with(&mut type_folder);
            typeid.push_str(&encode_ty(tcx, ty, &mut dict, encode_ty_options));
        }
        if !pushed_arg {
            // Empty parameter lists, whether declared as () or conventionally as (void), are
            // encoded with a void parameter specifier "v".
            typeid.push('v');
        }
    } else {
        for n in 0..fn_abi.fixed_count as usize {
            if fn_abi.args[n].mode == PassMode::Ignore {
                continue;
            }
            let ty = fn_abi.args[n].layout.ty.fold_with(&mut type_folder);
            typeid.push_str(&encode_ty(tcx, ty, &mut dict, encode_ty_options));
        }

        typeid.push('z');
    }

    // Close the "F..E" pair
    typeid.push('E');

    // Add encoding suffixes
    if options.contains(EncodeTyOptions::NORMALIZE_INTEGERS) {
        typeid.push_str(".normalized");
    }

    if options.contains(EncodeTyOptions::GENERALIZE_POINTERS) {
        typeid.push_str(".generalized");
    }

    typeid
}

/// Returns a type metadata identifier for the specified Instance using the Itanium C++ ABI with
/// vendor extended type qualifiers and types for Rust types that are not used at the FFI boundary.
pub fn typeid_for_instance<'tcx>(
    tcx: TyCtxt<'tcx>,
    mut instance: Instance<'tcx>,
    options: TypeIdOptions,
) -> String {
    if (matches!(instance.def, ty::InstanceDef::Virtual(..))
        && Some(instance.def_id()) == tcx.lang_items().drop_in_place_fn())
        || matches!(instance.def, ty::InstanceDef::DropGlue(..))
    {
        // Adjust the type ids of DropGlues
        //
        // DropGlues may have indirect calls to one or more given types drop function. Rust allows
        // for types to be erased to any trait object and retains the drop function for the original
        // type, which means at the indirect call sites in DropGlues, when typeid_for_fnabi is
        // called a second time, it only has information after type erasure and it could be a call
        // on any arbitrary trait object. Normalize them to a synthesized Drop trait object, both on
        // declaration/definition, and during code generation at call sites so they have the same
        // type id and match.
        //
        // FIXME(rcvalle): This allows a drop call on any trait object to call the drop function of
        //   any other type.
        //
        let def_id = tcx
            .lang_items()
            .drop_trait()
            .unwrap_or_else(|| bug!("typeid_for_instance: couldn't get drop_trait lang item"));
        let predicate = ty::ExistentialPredicate::Trait(ty::ExistentialTraitRef {
            def_id: def_id,
            args: List::empty(),
        });
        let predicates = tcx.mk_poly_existential_predicates(&[ty::Binder::dummy(predicate)]);
        let self_ty = Ty::new_dynamic(tcx, predicates, tcx.lifetimes.re_erased, ty::Dyn);
        instance.args = tcx.mk_args_trait(self_ty, List::empty());
    } else if let ty::InstanceDef::Virtual(def_id, _) = instance.def {
        let upcast_ty = match tcx.trait_of_item(def_id) {
            Some(trait_id) => trait_object_ty(
                tcx,
                ty::Binder::dummy(ty::TraitRef::from_method(tcx, trait_id, instance.args)),
            ),
            // drop_in_place won't have a defining trait, skip the upcast
            None => instance.args.type_at(0),
        };
        let stripped_ty = strip_receiver_auto(tcx, upcast_ty);
        instance.args = tcx.mk_args_trait(stripped_ty, instance.args.into_iter().skip(1));
    } else if let ty::InstanceDef::VTableShim(def_id) = instance.def
        && let Some(trait_id) = tcx.trait_of_item(def_id)
    {
        // VTableShims may have a trait method, but a concrete Self. This is not suitable for a vtable,
        // as the caller will not know the concrete Self.
        let trait_ref = ty::TraitRef::new(tcx, trait_id, instance.args);
        let invoke_ty = trait_object_ty(tcx, ty::Binder::dummy(trait_ref));
        instance.args = tcx.mk_args_trait(invoke_ty, trait_ref.args.into_iter().skip(1));
    }

    if !options.contains(EncodeTyOptions::USE_CONCRETE_SELF) {
        if let Some(impl_id) = tcx.impl_of_method(instance.def_id())
            && let Some(trait_ref) = tcx.impl_trait_ref(impl_id)
        {
            let impl_method = tcx.associated_item(instance.def_id());
            let method_id = impl_method
                .trait_item_def_id
                .expect("Part of a trait implementation, but not linked to the def_id?");
            let trait_method = tcx.associated_item(method_id);
            let trait_id = trait_ref.skip_binder().def_id;
            if traits::is_vtable_safe_method(tcx, trait_id, trait_method)
                && tcx.object_safety_violations(trait_id).is_empty()
            {
                // Trait methods will have a Self polymorphic parameter, where the concreteized
                // implementatation will not. We need to walk back to the more general trait method
                let trait_ref = tcx.instantiate_and_normalize_erasing_regions(
                    instance.args,
                    ty::ParamEnv::reveal_all(),
                    trait_ref,
                );
                let invoke_ty = trait_object_ty(tcx, ty::Binder::dummy(trait_ref));

                // At the call site, any call to this concrete function through a vtable will be
                // `Virtual(method_id, idx)` with appropriate arguments for the method. Since we have the
                // original method id, and we've recovered the trait arguments, we can make the callee
                // instance we're computing the alias set for match the caller instance.
                //
                // Right now, our code ignores the vtable index everywhere, so we use 0 as a placeholder.
                // If we ever *do* start encoding the vtable index, we will need to generate an alias set
                // based on which vtables we are putting this method into, as there will be more than one
                // index value when supertraits are involved.
                instance.def = ty::InstanceDef::Virtual(method_id, 0);
                let abstract_trait_args =
                    tcx.mk_args_trait(invoke_ty, trait_ref.args.into_iter().skip(1));
                instance.args = instance.args.rebase_onto(tcx, impl_id, abstract_trait_args);
            }
        } else if tcx.is_closure_like(instance.def_id()) {
            // We're either a closure or a coroutine. Our goal is to find the trait we're defined on,
            // instantiate it, and take the type of its only method as our own.
            let closure_ty = instance.ty(tcx, ty::ParamEnv::reveal_all());
            let (trait_id, inputs) = match closure_ty.kind() {
                ty::Closure(..) => {
                    let closure_args = instance.args.as_closure();
                    let trait_id = tcx.fn_trait_kind_to_def_id(closure_args.kind()).unwrap();
                    let tuple_args =
                        tcx.instantiate_bound_regions_with_erased(closure_args.sig()).inputs()[0];
                    (trait_id, Some(tuple_args))
                }
                ty::Coroutine(..) => match tcx.coroutine_kind(instance.def_id()).unwrap() {
                    hir::CoroutineKind::Coroutine(..) => (
                        tcx.require_lang_item(LangItem::Coroutine, None),
                        Some(instance.args.as_coroutine().resume_ty()),
                    ),
                    hir::CoroutineKind::Desugared(desugaring, _) => {
                        let lang_item = match desugaring {
                            hir::CoroutineDesugaring::Async => LangItem::Future,
                            hir::CoroutineDesugaring::AsyncGen => LangItem::AsyncIterator,
                            hir::CoroutineDesugaring::Gen => LangItem::Iterator,
                        };
                        (tcx.require_lang_item(lang_item, None), None)
                    }
                },
                ty::CoroutineClosure(..) => (
                    tcx.require_lang_item(LangItem::FnOnce, None),
                    Some(
                        tcx.instantiate_bound_regions_with_erased(
                            instance.args.as_coroutine_closure().coroutine_closure_sig(),
                        )
                        .tupled_inputs_ty,
                    ),
                ),
                x => bug!("Unexpected type kind for closure-like: {x:?}"),
            };
            let concrete_args = tcx.mk_args_trait(closure_ty, inputs.map(Into::into));
            let trait_ref = ty::TraitRef::new(tcx, trait_id, concrete_args);
            let invoke_ty = trait_object_ty(tcx, ty::Binder::dummy(trait_ref));
            let abstract_args = tcx.mk_args_trait(invoke_ty, trait_ref.args.into_iter().skip(1));
            // There should be exactly one method on this trait, and it should be the one we're
            // defining.
            let call = tcx
                .associated_items(trait_id)
                .in_definition_order()
                .find(|it| it.kind == ty::AssocKind::Fn)
                .expect("No call-family function on closure-like Fn trait?")
                .def_id;

            instance.def = ty::InstanceDef::Virtual(call, 0);
            instance.args = abstract_args;
        }
    }

    let fn_abi = tcx
        .fn_abi_of_instance(tcx.param_env(instance.def_id()).and((instance, ty::List::empty())))
        .unwrap_or_else(|error| {
            bug!("typeid_for_instance: couldn't get fn_abi of instance {instance:?}: {error:?}")
        });

    typeid_for_fnabi(tcx, fn_abi, options)
}

fn strip_receiver_auto<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Ty<'tcx> {
    let ty::Dynamic(preds, lifetime, kind) = ty.kind() else {
        bug!("Tried to strip auto traits from non-dynamic type {ty}");
    };
    if preds.principal().is_some() {
        let filtered_preds =
            tcx.mk_poly_existential_predicates_from_iter(preds.into_iter().filter(|pred| {
                !matches!(pred.skip_binder(), ty::ExistentialPredicate::AutoTrait(..))
            }));
        Ty::new_dynamic(tcx, filtered_preds, *lifetime, *kind)
    } else {
        // If there's no principal type, re-encode it as a unit, since we don't know anything
        // about it. This technically discards the knowledge that it was a type that was made
        // into a trait object at some point, but that's not a lot.
        tcx.types.unit
    }
}

#[instrument(skip(tcx), ret)]
fn trait_object_ty<'tcx>(tcx: TyCtxt<'tcx>, poly_trait_ref: ty::PolyTraitRef<'tcx>) -> Ty<'tcx> {
    assert!(!poly_trait_ref.has_non_region_param());
    let principal_pred = poly_trait_ref.map_bound(|trait_ref| {
        ty::ExistentialPredicate::Trait(ty::ExistentialTraitRef::erase_self_ty(tcx, trait_ref))
    });
    let mut assoc_preds: Vec<_> = traits::supertraits(tcx, poly_trait_ref)
        .flat_map(|super_poly_trait_ref| {
            tcx.associated_items(super_poly_trait_ref.def_id())
                .in_definition_order()
                .filter(|item| item.kind == ty::AssocKind::Type)
                .map(move |assoc_ty| {
                    super_poly_trait_ref.map_bound(|super_trait_ref| {
                        let alias_ty = ty::AliasTy::new(tcx, assoc_ty.def_id, super_trait_ref.args);
                        let resolved = tcx.normalize_erasing_regions(
                            ty::ParamEnv::reveal_all(),
                            alias_ty.to_ty(tcx),
                        );
                        debug!("Resolved {:?} -> {resolved}", alias_ty.to_ty(tcx));
                        ty::ExistentialPredicate::Projection(ty::ExistentialProjection {
                            def_id: assoc_ty.def_id,
                            args: ty::ExistentialTraitRef::erase_self_ty(tcx, super_trait_ref).args,
                            term: resolved.into(),
                        })
                    })
                })
        })
        .collect();
    assoc_preds.sort_by(|a, b| a.skip_binder().stable_cmp(tcx, &b.skip_binder()));
    let preds = tcx.mk_poly_existential_predicates_from_iter(
        iter::once(principal_pred).chain(assoc_preds.into_iter()),
    );
    Ty::new_dynamic(tcx, preds, tcx.lifetimes.re_erased, ty::Dyn)
}
