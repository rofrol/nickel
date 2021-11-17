use crate::cache::ImportResolver;
use crate::error::ImportError;
use crate::eval::{Closure, Environment, IdentKind, Thunk};
use crate::identifier::Ident;
use crate::term::{Contract, RichTerm, Term};
use crate::types::{AbsType, TypeAliasEnv, Types};
use codespan::FileId;
use simple_counter::*;
use std::path::PathBuf;

generate_counter!(FreshVarCounter, usize);

/// Share normal form.
///
/// Replace the subexpressions of WHNFs that are not functions by thunks, such that they can be
/// shared. It is similar to the behavior of other lazy languages with respect to data
/// constructors.  To do so, subexpressions are replaced by fresh variables, introduced by new let
/// bindings put at the beginning of the WHNF.
///
/// For example, take the expression
/// ```text
/// let x = {a = (1 + 1);} in x.a + x.a
/// ```
///
/// The term `{a = 1 + 1;}` is a record, and hence a WHNF. In consequence, the thunk allocated to x
/// is never updated. Without additional machinery, `a` will be recomputed each time is it used,
/// two times here.
///
/// The transformation replaces such subexpressions, namely the content of the fields
/// of records and the elements of lists - `(1 + 1)` in our example -, with fresh variables
/// introduced by `let`  added at the head of the term:
///
/// ```text
/// let x = (let var = 1 + 1 in {a = var;}) in x.a + x.a
/// ```
///
/// Now, the field `a` points to the thunk introduced by `var`: at the evaluation of the first
/// occurrence of `x.a`, this thunk is updated with `2`, and is not recomputed the second time.
///
/// Newly introduced variables begin with a special character to avoid clashing with user-defined
/// variables.
pub mod share_normal_form {
    use super::fresh_var;
    use crate::identifier::Ident;
    use crate::position::TermPos;
    use crate::term::{MetaValue, RichTerm, Term};
    use crate::types::TypeAliasEnv;

    /// Transform the top-level term of an AST to a share normal form, if it can.
    ///
    /// This function is not recursive: it just tries to apply one step of the transformation to
    /// the top-level node of the AST. For example, it transforms `[1 + 1, [1 + 2]]` to `let %0 = 1
    /// + 1 in [%0, [1 + 2]]`: the nested subterm `[1 + 2]` is left as it was. If the term is
    /// neither a record, a list nor an enriched value, it is returned the same.  In other words,
    /// the transformation is implemented as rewrite rules, and must be used in conjunction a
    /// traversal to obtain a full transformation.
    pub fn transform_one(rt: RichTerm, typeenv: &mut TypeAliasEnv) -> RichTerm {
        let RichTerm { term, pos } = rt;
        match *term {
            Term::Record(map, attrs) => {
                let mut bindings = Vec::with_capacity(map.len());

                let map = map
                    .into_iter()
                    .map(|(id, t)| {
                        if should_share(&t.term) {
                            let fresh_var = fresh_var();
                            let pos_t = t.pos;
                            bindings.push((fresh_var.clone(), t));
                            (id, RichTerm::new(Term::Var(fresh_var), pos_t))
                        } else {
                            (id, t)
                        }
                    })
                    .collect();

                with_bindings(Term::Record(map, attrs), bindings, pos)
            }
            Term::RecRecord(map, dyn_fields, attrs) => {
                // When a recursive record is evaluated, all fields need to be turned to closures
                // anyway (see the corresponding case in `eval::eval()`), which is what the share
                // normal form transformation does. This is why the test is more lax here than for
                // other constructors: it is not only about sharing, but also about the future
                // evaluation of recursive records. Only constant are not required to be
                // closurized.
                let mut bindings = Vec::with_capacity(map.len());

                let map = map
                    .into_iter()
                    .map(|(id, t)| {
                        if !t.as_ref().is_constant() {
                            let fresh_var = fresh_var();
                            let pos_t = t.pos;
                            bindings.push((fresh_var.clone(), t));
                            (id, RichTerm::new(Term::Var(fresh_var), pos_t))
                        } else {
                            (id, t)
                        }
                    })
                    .collect();

                let dyn_fields = dyn_fields
                    .into_iter()
                    .map(|(id_t, t)| {
                        if !t.as_ref().is_constant() {
                            let fresh_var = fresh_var();
                            let pos_t = t.pos;
                            bindings.push((fresh_var.clone(), t));
                            (id_t, RichTerm::new(Term::Var(fresh_var), pos_t))
                        } else {
                            (id_t, t)
                        }
                    })
                    .collect();

                with_bindings(Term::RecRecord(map, dyn_fields, attrs), bindings, pos)
            }
            Term::List(ts) => {
                let mut bindings = Vec::with_capacity(ts.len());

                let ts = ts
                    .into_iter()
                    .map(|t| {
                        if should_share(&t.term) {
                            let fresh_var = fresh_var();
                            let pos_t = t.pos;
                            bindings.push((fresh_var.clone(), t));
                            RichTerm::new(Term::Var(fresh_var), pos_t)
                        } else {
                            t
                        }
                    })
                    .collect();

                with_bindings(Term::List(ts), bindings, pos)
            }
            Term::MetaValue(mut meta @ MetaValue { value: Some(_), .. }) => {
                if meta.value.as_ref().map(|t| should_share(&t.term)).unwrap() {
                    let fresh_var = fresh_var();
                    let t = meta.value.take().unwrap();
                    meta.value
                        .replace(RichTerm::new(Term::Var(fresh_var.clone()), t.pos));
                    let inner = RichTerm::new(Term::MetaValue(meta), pos);
                    RichTerm::new(Term::Let(fresh_var, t, inner), pos)
                } else {
                    RichTerm::new(Term::MetaValue(meta), pos)
                }
            }
            t => RichTerm::new(t, pos),
        }
    }

    /// Determine if a subterm of a WHNF should be wrapped in a thunk in order to be shared.
    ///
    /// Sharing is typically useless if the subterm is already a WHNF which can be copied without
    /// duplicating any work. On the other hand, a WHNF which can contain other shareable
    /// subexpressions, such as a record, should be shared.
    fn should_share(t: &Term) -> bool {
        match t {
            Term::Null
            | Term::Bool(_)
            | Term::Num(_)
            | Term::Str(_)
            | Term::Lbl(_)
            | Term::Sym(_)
            | Term::Var(_)
            | Term::Enum(_)
            | Term::Fun(_, _) => false,
            _ => true,
        }
    }

    /// Bind a list of pairs `(identifier, term)` in a term.
    ///
    /// Given the term `body` and bindings of identifiers to terms represented as a list of pairs
    /// `(id_1, term_1), .., (id_n, term_n)`, return the new term `let id_n = term_n in ... let
    /// id_1 = term_1 in body`.
    fn with_bindings(body: Term, bindings: Vec<(Ident, RichTerm)>, pos: TermPos) -> RichTerm {
        bindings.into_iter().fold(
            RichTerm {
                term: Box::new(body),
                pos: pos.into_inherited(),
            },
            |acc, (id, t)| RichTerm::new(Term::Let(id, t, acc), pos),
        )
    }
}

pub mod import_resolution {
    use super::{ImportResolver, PathBuf, RichTerm, Term};
    use crate::error::ImportError;

    /// Resolve the import if the term is an unresolved import, or return the term unchanged.
    /// As [`share_normal_form::transform_one`](../share_normal_form/fn.transform_one.html),
    /// this function is not recursive.
    pub fn transform_one<R>(
        rt: RichTerm,
        resolver: &mut R,
        parent: &Option<PathBuf>,
    ) -> Result<RichTerm, ImportError>
    where
        R: ImportResolver,
    {
        let term = rt.as_ref();
        match term {
            Term::Import(path) => {
                let (_, file_id) = resolver.resolve(&path, parent.clone(), &rt.pos)?;
                Ok(RichTerm::new(Term::ResolvedImport(file_id), rt.pos))
            }
            _ => Ok(rt),
        }
    }
}

/// During the evaluation, we the following invariant is enforced: any contract (be it the type
/// annotation, or the contracts) contained in a `MetaValue` must have been applied to the inner
/// value of this metavalue. This invariant is false just after parsing, as there's merely no
/// direct `Assume` in the output AST. This transformation makes it true after program
/// transformations by generating corresponding assume.
///
/// It must be run before `share_normal_form` to avoid rechecking contracts each time the inner
/// value is unwrapped.
pub mod apply_contracts {
    use super::{RichTerm, Term};
    use crate::mk_app;
    use crate::types::{AbsType, TypeAliasEnv, Types};

    /// If the top-level node of the AST is a meta-value, apply the meta-value's contracts to the
    /// inner value.  Otherwise, return the term unchanged.
    pub fn transform_one(rt: RichTerm, typeenv: &mut TypeAliasEnv) -> RichTerm {
        let RichTerm { term, pos } = rt;

        match *term {
            Term::MetaValue(mut meta) if meta.value.is_some() => {
                let inner = meta.types.iter().chain(meta.contracts.iter()).fold(
                    meta.value.take().unwrap(),
                    |acc, ctr| {
                        let (ctr_ty, ctr_lbl) =
                            if let Types(AbsType::Var(ident)) = ctr.types.clone() {
                                if let Some(ty_replace) = typeenv.get(&ident) {
                                    (ty_replace, ctr.label.clone())
                                } else {
                                    (ctr.types.clone(), ctr.label.clone())
                                }
                            } else {
                                (ctr.types.clone(), ctr.label.clone())
                            };
                        mk_app!(ctr_ty.contract(), Term::Lbl(ctr_lbl), acc).with_pos(pos)
                    },
                );

                meta.value.replace(inner);
                RichTerm::new(Term::MetaValue(meta), pos)
            }
            v @ Term::TypeAlias(_, _) => {
                if let Term::TypeAlias(id, ty) = v.clone() {
                    typeenv.insert(id.clone(), ty.clone());
                } else {
                    panic!();
                }
                RichTerm::new(v, pos)
            }
            t => RichTerm::new(t, pos),
        }
    }
}

/// The state passed around during the imports resolution. It holds a reference to the import
/// resolver, to a stack of pending imported term to be transformed and the path of the import
/// currently being processed, if any.
struct ImportsResolutionState<'a, R> {
    resolver: &'a mut R,
    stack: &'a mut Vec<FileId>,
    parent: Option<PathBuf>,
}

/// Apply all program transformations, which are currently the share normal form transformations and
/// contracts application.
/// Do not perform transformation on the imported files.
/// If needed, either do it yourself using pending imports returned by
/// [`resolve_imports`](../fn.resolve_imports.html)
/// or use the [`Cache`](../../cache/struct.Cache.html)
pub fn transform(rt: RichTerm) -> RichTerm {
    rt.traverse(
        &mut |rt: RichTerm, typeenv| -> Result<RichTerm, ()> {
            // We need to do contract generation before wrapping stuff in variables
            let rt = apply_contracts::transform_one(rt, typeenv);
            let rt = share_normal_form::transform_one(rt, typeenv);
            Ok(rt)
        },
        &mut TypeAliasEnv::new(),
    )
    .unwrap()
}

/// import resolution.
///
/// All resolved imports are stacked during the process. Once the term has been traversed,
/// the elements of this stack are returned. The caller is responsible
/// to recursively resolve imports of this stack and or to perform
/// transformations on it.
pub fn resolve_imports<R>(
    rt: RichTerm,
    resolver: &mut R,
) -> Result<(RichTerm, Vec<FileId>), ImportError>
where
    R: ImportResolver,
{
    let mut stack = Vec::new();

    let source_file: Option<PathBuf> = rt.pos.as_opt_ref().map(|x| {
        let path = resolver.get_path(x.src_id);
        PathBuf::from(path)
    });
    let result = imports_pass(rt, resolver, &mut stack, source_file)?;

    Ok((result, stack))
}

/// Perform one full imports resolution pass. Put all imports encountered for the first time in
/// `stack`, but do not process them.
fn imports_pass<R>(
    rt: RichTerm,
    resolver: &mut R,
    stack: &mut Vec<FileId>,
    parent: Option<PathBuf>,
) -> Result<RichTerm, ImportError>
where
    R: ImportResolver,
{
    let mut state = ImportsResolutionState {
        resolver,
        stack,
        parent,
    };

    // If an import is resolved, then stack it.
    rt.traverse(
        &mut |rt: RichTerm,
              state: &mut ImportsResolutionState<R>|
         -> Result<RichTerm, ImportError> {
            let rt = import_resolution::transform_one(rt, state.resolver, &state.parent)?;

            if let Term::ResolvedImport(file_id) = rt.term.as_ref() {
                state.stack.push(*file_id);
            }
            Ok(rt)
        },
        &mut state,
    )
}

/// Generate a new fresh variable which do not clash with user-defined variables.
pub fn fresh_var() -> Ident {
    Ident(format!("%{}", FreshVarCounter::next()))
}

/// Structures which can be packed together with their environment as a closure.
///
/// The typical implementer is [`RichTerm`](../term/enum.RichTerm.html), but structures containing
/// terms can also be closurizable, such as the contract in a [`Types`](../types/typ.Types.html).
/// In this case, the inner term is closurized.
pub trait Closurizable {
    /// Pack a closurizable together with its environment `with_env` as a closure in the main
    /// environment `env`.
    fn closurize(self, env: &mut Environment, with_env: Environment) -> Self;
}

impl Closurizable for RichTerm {
    /// Pack a term together with an environment as a closure.
    ///
    /// Generate a fresh variable, bind it to the corresponding closure `(t,with_env)` in `env`,
    /// and return this variable as a fresh term.
    fn closurize(self, env: &mut Environment, with_env: Environment) -> RichTerm {
        let var = fresh_var();
        let pos = self.pos;
        let closure = Closure {
            body: self,
            env: with_env,
        };
        env.insert(var.clone(), Thunk::new(closure, IdentKind::Record()));

        RichTerm::new(Term::Var(var), pos.into_inherited())
    }
}

impl Closurizable for Types {
    /// Pack the contract of a type together with an environment as a closure.
    ///
    /// Extract the underlying contract, closurize it and wrap it back as a flat type (an opaque
    /// type defined by a custom contract).
    fn closurize(self, env: &mut Environment, with_env: Environment) -> Types {
        Types(AbsType::Flat(self.contract().closurize(env, with_env)))
    }
}

impl Closurizable for Contract {
    fn closurize(self, env: &mut Environment, with_env: Environment) -> Contract {
        Contract {
            types: self.types.closurize(env, with_env),
            label: self.label,
        }
    }
}
