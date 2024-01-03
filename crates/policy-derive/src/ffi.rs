use std::{collections::HashSet, fs::File, io::Write};

use policy_lang::{
    ast::{AstNode, FieldDefinition, FunctionDecl, StructDefinition, VType},
    lang,
};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, ToTokens};
use syn::{
    parse::{Parse, ParseStream},
    parse_quote,
    spanned::Spanned,
    Attribute, Error, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, LitStr, Meta, Pat, PatIdent,
    PatType, Path, ReturnType, Token, Type,
};

use crate::attr::{get_lit_str, Attr, Symbol};

// TODO(eric): allow `#[ffi_export("foo")]` as an alternative to
// `#[ffi_export(name = "foo")]`?

pub(crate) fn parse(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let FfiAttr { module, structs } = syn::parse2(attr)?;
    let mut item: ItemImpl = syn::parse2(item)?;
    // The name of the type that the `#[ffi]` attribute is
    // applied to.
    let ident = type_name(&item.self_ty)?;
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();

    // Checks for duplicate FFI names.
    let mut ext_names = HashSet::new();

    let mut funcs = Vec::<Func>::new();
    for item in &mut item.items {
        let ImplItem::Fn(ref mut f) = item else {
            continue;
        };
        if let Some(f) = Func::from_ast(f)? {
            let name = f.ext_name.to_string();
            if !ext_names.insert(name) {
                return Err(Error::new_spanned(
                    item,
                    format!("duplicate FFI function name: {}", f.ext_name),
                ));
            }
            funcs.push(f);
        }
    }

    // Our `extern crate`s.
    let alloc: Path = parse_quote!(_alloc);
    let crypto: Path = parse_quote!(_crypto);
    let vm: Path = parse_quote!(_policy_vm);

    // `struct Foo { ... }` definitions as parsed from
    // `#[ffi(def = "...")]`.
    let structs = structs.iter().map(|d| {
        let name = format_ident!("{}", d.identifier);
        let names = d
            .fields
            .iter()
            .map(|d| format_ident!("{}", d.identifier))
            .collect::<Vec<_>>();
        let fields = d
            .fields
            .iter()
            .map(|d| format_ident!("__field_{}", d.identifier))
            .collect::<Vec<_>>();
        let types = d.fields.iter().map(|d| {
            let vtype = TypeTokens::new(&d.field_type, &alloc, &crypto, &vm);
            quote!(#vtype)
        });
        quote! {
            #[must_use]
            #[derive(Clone, Debug, Eq, PartialEq)]
            pub struct #name {
                #(pub #names: #types),*
            }
            #[automatically_derived]
            impl ::core::convert::From<#name> for #vm::Value {
                fn from(__value: #name) -> Self {
                    let __struct = #vm::Struct::new(
                        ::core::stringify!(#name),
                        &[
                            #(#vm::KVPair::new(
                                ::core::stringify!(#names),
                                __value.#names.into(),
                            )),*,
                        ],
                    );
                    __struct.into()
                }
            }
            #[automatically_derived]
            impl ::core::convert::TryFrom<#vm::Value> for #name {
                type Error = #vm::MachineErrorType;

                fn try_from(__value: #vm::Value) -> ::core::result::Result<Self, Self::Error> {
                    let #vm::Value::Struct(mut __struct) = __value else {
                        return ::core::result::Result::Err(
                            #vm::MachineErrorType::InvalidType);
                    };
                    if __struct.name != ::core::stringify!(#name) {
                        return ::core::result::Result::Err(
                            #vm::MachineErrorType::InvalidType);
                    }
                    #(
                        let #fields = __struct.fields.remove(::core::stringify!(#names))
                            .ok_or(#vm::MachineErrorType::InvalidStructMember(
                                    #alloc::string::String::from(::core::stringify!(#names)),
                            ))?;
                    )*
                    if !__struct.fields.is_empty() {
                        return ::core::result::Result::Err(#vm::MachineErrorType::BadState);
                    }
                    ::core::result::Result::Ok(#name {
                        #(
                            #names: ::core::convert::TryFrom::<#vm::Value>::try_from(#fields)?
                        ),*
                    })
                }
            }
        }
    });

    // The implementation of `FfiModule`.
    let mod_impl = {
        // The `Func` variant identifiers:
        //    __some_func,
        //    __another_func,
        //    ...
        let variants = funcs.iter().map(|f| f.variant()).collect::<Vec<_>>();

        // The `__Func` variants mapped from `usize`:
        //    const __some_func = Self::some_func as usize;
        //    const __another_func = Self::another_func as usize;
        //    ...
        let consts = variants.iter().map(|variant| {
            quote! {
                const #variant: usize = __Func::#variant as usize;
            }
        });

        // The `Func` variant identifiers mapped from
        // `usize`:
        //    Func_some_func => Some(Func_some_func),
        //    Func_another_func => Some(Func_another_func),
        //    ...
        //    _ => None,
        let mappings = variants.iter().map(|variant| {
            quote! {
                #variant => __Func::#variant
            }
        });

        // The `Func::call` match cases.
        //
        //    Func::__Func0 => { foo() },
        //    Func::__Func1 => { let v = stack.pop()?; bar(v) }
        //    ...
        let cases = funcs.iter().map(|f| {
            let variant = &f.variant();

            let names = f
                .args
                .iter()
                .map(|arg| format_ident!("__arg_{}", arg.ident));
            let args = f
                .args
                .iter()
                .map(|arg| {
                    let name = format_ident!("__arg_{}", arg.ident);
                    let rtype = &arg.ty.ty;
                    quote! {
                        let #name = #vm::Stack::pop::<#rtype>(__stack)?
                    }
                })
                // Arguments are pushed to the stack in argument
                // order, so pop them in reverse order.
                .rev();

            let name = &f.name;
            let name = if f.is_method {
                quote!(self.#name)
            } else {
                quote!(Self::#name)
            };

            let inner = match &f.result {
                (ReturnType::Default, Color::Finish) => quote! {
                    #(#args);*;
                    let _: () = #name(__ctx, #(#names),*);
                    ::core::result::Result::Ok(())
                },
                // Rust returns `Result<(), E>`.
                (ReturnType::Type(_, _), Color::Finish) => quote! {
                    #(#args);*;
                    let _: () = #name(__ctx, #(#names),*)?;
                    ::core::result::Result::Ok(())
                },
                // Rust returns `Result<T, E>`.
                (ReturnType::Type(_, _), Color::Pure(_)) => quote! {
                    #(#args);*;
                    let __result = #name(__ctx, #(#names),*)?;
                    #vm::Stack::push(__stack, __result)?;
                    ::core::result::Result::Ok(())
                },
                // These conditions should have been covered in
                // `Func::from_ast`.
                _ => unreachable!("invalid result"),
            };
            quote! {
                __Func::#variant => { #inner }
            }
        });

        // The function table.
        let funcs = funcs.iter().map(|f| {
            let name = format!("{module}::{}", f.ext_name);
            let args = f.args.iter().map(|arg| {
                let name = arg.def.identifier.clone();
                let vtype = VTypeTokens::new(&arg.def.field_type);
                quote!(#vm::arg!(#name, #vtype))
            });
            let color = match &f.result {
                (ReturnType::Default, _) | (_, Color::Finish) => quote! {
                    #vm::ffi::Color::Finish
                },
                (_, Color::Pure(vtype)) => {
                    let vtype = VTypeTokens::new(vtype);
                    quote!(#vm::ffi::Color::Pure(#vm::ffi::Type::#vtype))
                }
            };
            quote! {
                #vm::ffi::Func {
                    name: #name,
                    args: &[#(#args),*],
                    color: #color,
                }
            }
        });

        quote! {
            #[automatically_derived]
            impl #impl_generics #vm::ffi::FfiModule for #ident #ty_generics #where_clause {
                type Error = #vm::MachineError;

                const SCHEMA: #vm::ffi::ModuleSchema<'static> = #vm::ffi::ModuleSchema {
                    name: #module,
                    functions: &[
                        #(#funcs),*
                    ],
                };

                #[doc(hidden)]
                #[allow(non_snake_case)]
                fn call<__E: #crypto::Engine + ?Sized>(
                    &mut self,
                    __proc: usize,
                    __stack: &mut impl #vm::Stack,
                    __ctx: &mut #vm::CommandContext<'_, __E>,
                ) -> ::core::result::Result<(), Self::Error> {
                    #[allow(non_camel_case_types, clippy::enum_variant_names)]
                    enum __Func {
                        #(#variants),*
                    }
                    #[allow(non_upper_case_globals)]
                    let __f = {
                        #(#consts);*
                        match __proc {
                            #(#mappings),*,
                            _ => {
                                return ::core::result::Result::Err(
                                    #vm::MachineError::new(#vm::MachineErrorType::FfiProcedureNotDefined(
                                        Self::SCHEMA.name.to_owned(),
                                        __proc,
                                )));
                            }
                        }
                    };
                    // TODO(eric): instead of making this
                    // gigantic function, create a smaller
                    // function for each case and let the
                    // compiler decide whether they should be
                    // inlined.
                    match __f {
                        #(#cases),*
                    }
                }
            }
        }
    };

    let module = format_ident!("__{module}_ffi");
    let block = quote! {
        #[doc(hidden)]
        #[allow(missing_docs, unused_extern_crates)]
        mod #module {
            #[allow(clippy::clippy::wildcard_imports)]
            use super::*;

            // TODO(eric): make `alloc` optional.
            extern crate alloc as #alloc;
            extern crate crypto as #crypto;
            extern crate policy_vm as #vm;

            #(#structs)*
        }

        #[doc(hidden)]
        #[allow(missing_docs, unused_extern_crates)]
        const _: () = {
            #[allow(clippy::clippy::wildcard_imports)]
            use #module::*;

            // TODO(eric): somehow move this out of the const
            // wrapper. The trick is rewriting function
            // arguments/results that use generated structs.
            #item

            // TODO(eric): make `alloc` optional.
            extern crate alloc as #alloc;
            extern crate crypto as #crypto;
            extern crate policy_vm as #vm;

            #mod_impl
        };
    };

    // Undocumented.
    if cfg!(policy_derive_debug) {
        let mut data = block.to_string();
        if let Ok(file) = syn::parse_file(&data) {
            data = prettyplease::unparse(&file);
        }
        File::create("/tmp/expand.rs")
            .expect("unable to create `/tmp/expand.rs`")
            .write_all(data.as_bytes())
            .expect("unable to write all data to `/tmp/expand.rs`");
    }
    Ok(block)
}

mod kw {
    syn::custom_keyword!(module);
    syn::custom_keyword!(def);
}

const MODULE: Symbol = Symbol("name");
const DEF: Symbol = Symbol("def");

/// The `#[ffi]` attribute.
struct FfiAttr {
    module: String,
    structs: Vec<AstNode<StructDefinition>>,
}

impl Parse for FfiAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut module = Attr::none(MODULE);
        let mut def = Attr::none(DEF);

        while !input.is_empty() {
            let lookahead = input.lookahead1();
            // `module = "..."`
            if lookahead.peek(kw::module) {
                input.parse::<kw::module>()?;
                let _: Token![=] = input.parse()?;
                let name: LitStr = input.parse()?;
                skip_comma(input)?;
                module.set(&name, name.value())?;
            // `def = "..."`
            } else if lookahead.peek(kw::def) {
                input.parse::<kw::def>()?;
                let _: Token![=] = input.parse()?;
                let decl: LitStr = input.parse()?;
                skip_comma(input)?;
                let structs = lang::parse_ffi_structs(&decl.value()).map_err(|err| {
                    Error::new(decl.span(), format!("invalid policy definition: {err}"))
                })?;
                def.set(&decl, structs)?;
            } else {
                return Err(lookahead.error());
            }
        }

        let module = module
            .get()
            .ok_or(Error::new(input.span(), "missing `{MODULE}` argument"))?;
        Ok(Self {
            module,
            structs: def.get().unwrap_or_default(),
        })
    }
}

/// Skips the next token if it's a comma.
fn skip_comma(input: ParseStream<'_>) -> syn::Result<()> {
    let lookahead = input.lookahead1();
    if lookahead.peek(Token![,]) {
        let _: Token![,] = input.parse()?;
    }
    Ok(())
}

/// Returns the type's name,
fn type_name(ty: &Type) -> syn::Result<Ident> {
    match ty {
        // TODO(eric): this is probably wrong for something like
        // `crate::foo::bar::Baz`.
        Type::Path(path) => Ok(path.path.segments[0].ident.clone()),
        _ => Err(Error::new(ty.span(), "unknown type")),
    }
}

const FFI_EXPORT: Symbol = Symbol("ffi_export");

/// The `#[ffi_export]` attribute.
struct FfiExportAttr {
    def: FunctionDecl,
}

impl FfiExportAttr {
    fn new(span: Span, attrs: &mut Vec<Attribute>) -> syn::Result<Option<Self>> {
        let mut def = Attr::none(DEF);

        let mut found = attrs
            .iter()
            .enumerate()
            .filter(|(_, attr)| attr.path() == FFI_EXPORT);
        let Some((idx, attr)) = found.next() else {
            return Ok(None);
        };
        if found.next().is_some() {
            return Err(Error::new(
                span,
                format!("`{FFI_EXPORT}` attribute can only be used once"),
            ));
        }
        match &attr.meta {
            // An empty attribute: `#[ffi_export]`.
            Meta::Path(_) => {}
            _ => attr.parse_nested_meta(|meta| {
                if meta.path == DEF {
                    let decl = get_lit_str(DEF, &meta)?.value();
                    let fd = lang::parse_ffi_decl(&decl)
                        .map_err(|err| meta.error(format!("invalid policy definition: {err}")))?;
                    def.set(&meta.path, fd)
                } else {
                    let path = meta.path.to_token_stream().to_string().replace(' ', "");
                    Err(meta.error(format!("unknown attr: {path}")))
                }
            })?,
        };
        attrs.remove(idx);

        let def = def
            .get()
            .ok_or(Error::new(span, "missing `def` argument in attribute"))?;

        Ok(Some(Self { def }))
    }
}

/// A foreign function.
#[derive(Clone, Debug)]
struct Func {
    /// The function's name (in Rust).
    name: Ident,
    /// The function's name (in Policy code).
    ext_name: Ident,
    /// Is this a method or associated function?
    is_method: bool,
    /// The function's arguments.
    args: Vec<Arg>,
    /// The function's result type.
    result: (ReturnType, Color),
}

impl Func {
    fn from_ast(item: &mut ImplItemFn) -> syn::Result<Option<Self>> {
        let attr = match FfiExportAttr::new(item.span(), &mut item.attrs)? {
            Some(v) => v,
            None => return Ok(None),
        };
        let name = item.sig.ident.clone();

        // TODO(eric): reject ext names with invalid characters,
        // including "::".
        let ext_name = format_ident!("{}", attr.def.identifier);

        let is_method = item
            .sig
            .inputs
            .iter()
            .any(|arg| matches!(arg, FnArg::Receiver(_)));

        // The second argument is `&CommandContext`, which is
        // passed in by `call`, so skip them.
        //
        // TODO(eric): we should issue a diagnostic when the
        // first non-self argument isn't `&CommandContext`.
        let num_skip = if is_method { 2 } else { 1 };
        let num_args = match item.sig.inputs.len().checked_sub(num_skip) {
            Some(n) => n,
            None => {
                return Err(Error::new_spanned(
                    &item.sig,
                    format!(
                        "too few function arguments: {} < {num_skip}",
                        item.sig.inputs.len()
                    ),
                ))
            }
        };
        let num_def_args = attr.def.arguments.len();
        if num_args != num_def_args {
            return Err(Error::new_spanned(
                &item.sig,
                format!("incorrect number of arguments per `def`: found {num_args}, want {num_def_args}"),
            ));
        }

        let args = item
            .sig
            .inputs
            .iter()
            .skip(num_skip)
            .zip(attr.def.arguments.iter())
            .map(|(arg, def)| match arg {
                FnArg::Receiver(_) => unreachable!("should have skipped the receiver"),
                FnArg::Typed(t) => {
                    let Pat::Ident(PatIdent { ident, .. }) = &*t.pat else {
                        return Err(Error::new_spanned(
                            arg,
                            format!("invalid argument name: {}", t.pat.to_token_stream()),
                        ));
                    };
                    Ok(Arg {
                        ident: ident.clone(),
                        ty: t.clone(),
                        def: def.clone(),
                    })
                }
            })
            .collect::<syn::Result<Vec<_>>>()?;

        let color = match attr.def.return_type.clone() {
            Some(v) => Color::Pure(v),
            None => Color::Finish,
        };
        let result = match (&item.sig.output, color) {
            (ReturnType::Default, Color::Pure(vtype)) => {
                return Err(Error::new(
                    item.span(),
                    format!("Rust function returns `()`, but FFI function returns `{vtype}`"),
                ));
            }
            (ty, color) => (ty.clone(), color),
        };

        Ok(Some(Self {
            name,
            ext_name,
            is_method,
            args,
            result,
        }))
    }

    fn variant(&self) -> Ident {
        format_ident!("__{}", self.name)
    }
}

/// A function argument.
#[derive(Clone, Debug)]
struct Arg {
    ident: Ident,
    ty: PatType,
    def: FieldDefinition,
}

#[derive(Clone, Debug)]
enum Color {
    Finish,
    Pure(VType),
}

/// Implements [`ToTokens`] for `VType.`
struct VTypeTokens<'a> {
    vtype: &'a VType,
}

impl<'a> VTypeTokens<'a> {
    fn new(vtype: &'a VType) -> Self {
        Self { vtype }
    }
}

impl ToTokens for VTypeTokens<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let item = match self.vtype {
            VType::String => quote!(String),
            VType::Bytes => quote!(Bytes),
            VType::Int => quote!(Int),
            VType::Bool => quote!(Bool),
            VType::Id => quote!(Id),
            VType::Struct(name) => quote!(Struct(#name)),
            VType::Optional(vtype) => {
                let vtype = VTypeTokens::new(vtype);
                quote!(Optional(&#vtype))
            }
        };
        tokens.extend(item)
    }
}

/// An implementation of [`ToTokens`] that maps [`VType`] to its
/// corresponding Rust types.
struct TypeTokens<'a> {
    vtype: &'a VType,
    alloc: &'a Path,
    crypto: &'a Path,
    vm: &'a Path,
}

impl<'a> TypeTokens<'a> {
    fn new(vtype: &'a VType, alloc: &'a Path, crypto: &'a Path, vm: &'a Path) -> Self {
        Self {
            vtype,
            alloc,
            crypto,
            vm,
        }
    }
}

impl ToTokens for TypeTokens<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let alloc = self.alloc;
        let crypto = self.crypto;
        let vm = self.vm;
        let item = match self.vtype {
            VType::String => quote!(#alloc::string::String),
            VType::Bytes => quote!(#alloc::vec::Vec<u8>),
            VType::Int => quote!(i64),
            VType::Bool => quote!(bool),
            VType::Id => quote!(#crypto::Id),
            VType::Struct(name) => {
                let ident = format_ident!("{name}");
                quote!(#ident)
            }
            VType::Optional(vtype) => {
                let vtype = TypeTokens::new(vtype, alloc, crypto, vm);
                quote!(::core::option::Option<#vtype>)
            }
        };
        tokens.extend(item)
    }
}
