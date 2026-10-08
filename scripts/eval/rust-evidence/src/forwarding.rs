//! A retained declaration is evidence only when it forwards the original inputs.
use quote::ToTokens;
use syn::{Expr, FnArg, Ident, Item, ItemFn, Pat, ReturnType, Stmt, Type};

fn name(item: &Item) -> Option<&Ident> {
    match item {
        Item::Fn(x) => Some(&x.sig.ident),
        Item::Struct(x) => Some(&x.ident),
        Item::Enum(x) => Some(&x.ident),
        Item::Trait(x) => Some(&x.ident),
        Item::Type(x) => Some(&x.ident),
        Item::Const(x) => Some(&x.ident),
        Item::Static(x) => Some(&x.ident),
        _ => None,
    }
}

fn corresponding(candidate: &Item, retained: &Item) -> bool {
    match (candidate, retained) {
        (Item::Impl(a), Item::Impl(b)) => {
            a.self_ty.to_token_stream().to_string() == b.self_ty.to_token_stream().to_string()
        }
        _ => name(candidate).is_some_and(|n| Some(n) == name(retained)),
    }
}

pub(super) fn removed_or_forwarded(candidate: &Item, after: &[Item], module: &str) -> bool {
    after
        .iter()
        .filter(|item| corresponding(candidate, item))
        .all(|retained| match (candidate, retained) {
            (Item::Fn(candidate), Item::Fn(wrapper)) => forwards(wrapper, candidate, module),
            // A changed retained type/constant/impl is still a retained
            // implementation. There is no forwarding-body proof for it here.
            _ => false,
        })
}

fn forwards(wrapper: &ItemFn, candidate: &ItemFn, module: &str) -> bool {
    if !super::allowed(&wrapper.attrs, false)
        || wrapper.sig.to_token_stream().to_string() != candidate.sig.to_token_stream().to_string()
        || !wrapper.sig.generics.params.is_empty()
        || wrapper.sig.generics.where_clause.is_some()
        || wrapper.sig.asyncness.is_some()
        || wrapper.sig.unsafety.is_some()
    {
        return false;
    }
    // Decline generics/async/unsafe wrappers rather than guessing about name
    // resolution, await, or unsafe blocks. The accepted body is exactly one call.
    let [Stmt::Expr(expr, semi)] = wrapper.block.stmts.as_slice() else {
        return false;
    };
    let (expr, returns) = match expr {
        Expr::Return(r) if r.attrs.is_empty() => {
            let Some(expr) = &r.expr else {
                return false;
            };
            (expr.as_ref(), true)
        }
        expr => (expr, false),
    };
    if semi.is_some()
        && !returns
        && !matches!(&wrapper.sig.output, ReturnType::Default)
        && !matches!(&wrapper.sig.output, ReturnType::Type(_, ty) if matches!(ty.as_ref(), Type::Tuple(t) if t.elems.is_empty()))
    {
        return false;
    }
    let Expr::Call(call) = expr else {
        return false;
    };
    let Expr::Path(callee) = call.func.as_ref() else {
        return false;
    };
    if !call.attrs.is_empty()
        || !callee.attrs.is_empty()
        || callee.qself.is_some()
        || callee.path.leading_colon.is_some()
        || callee.path.segments.len() != 2
        || callee.path.segments[0].ident != module
        || callee.path.segments[1].ident != candidate.sig.ident
        || callee.path.segments.iter().any(|s| !s.arguments.is_empty())
        || call.args.len() != wrapper.sig.inputs.len()
    {
        return false;
    }
    wrapper
        .sig
        .inputs
        .iter()
        .zip(&call.args)
        .all(|(input, arg)| {
            let FnArg::Typed(input) = input else {
                return false;
            };
            let Pat::Ident(binding) = input.pat.as_ref() else {
                return false;
            };
            let Expr::Path(arg) = arg else {
                return false;
            };
            binding.attrs.is_empty()
                && binding.by_ref.is_none()
                && binding.subpat.is_none()
                && arg.attrs.is_empty()
                && arg.qself.is_none()
                && arg.path.is_ident(&binding.ident)
        })
}
