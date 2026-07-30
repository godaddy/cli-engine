//! `#[derive(EnvConfig)]` — see `cli_engine::env_config` for the trait and
//! runtime pieces this macro's generated code calls into.
//!
//! Per-field `#[env_config(...)]` attributes:
//!
//! - `key = "..."` — TOML key to look up (default: the field's Rust name).
//! - `env = "SUFFIX"` — opt-in environment-variable suffix.
//! - `default = <expr>` — literal fallback of the field's own type.
//! - `default_fn = <path>` — `fn(&SourceChain<'_>) -> T`, computed lazily.
//! - `from_toml = <path>` — `fn(&cli_engine::env_config::toml::Value) -> Result<T, String>`,
//!   replaces the default `T: DeserializeOwned` conversion. Name the
//!   parameter type through `cli_engine::env_config::toml` (re-exported)
//!   rather than a direct `toml` dependency of your own, so your crate
//!   doesn't need to track cli-engine's `toml` version.
//! - `from_env = <path>` — `fn(&str) -> Result<T, String>`, replaces the
//!   default `T: FromStr` conversion.
//! - `to_toml = <path>` — `fn(T) -> cli_engine::env_config::toml::Value`,
//!   replaces the default `T: Into<toml::Value>` conversion used when
//!   building an
//!   [`EnvTable`](../cli_engine/environments/struct.EnvTable.html) *from* an
//!   instance (see `impl From<Self> for EnvTable`, generated alongside
//!   `EnvConfig` so a compiled-in environment can be registered as a plain
//!   struct value via `Environments::with_environment`).
//! - `allow_blank` — bare marker (no value); by default, a source that
//!   answers with an empty-or-whitespace-only string is treated as not
//!   having answered at all, so the field keeps looking at the rest of the
//!   `SourceChain` (and ultimately falls to `default`/`default_fn`) instead
//!   of accepting `""` literally. This default fits nearly every field: a
//!   blank override is essentially always a mistake or an unset placeholder,
//!   never a real value. Set `allow_blank` on the rare field where an
//!   explicit `""` is itself a meaningful, literal answer distinct from
//!   "unset."
//!
//! `default` and `default_fn` are mutually exclusive.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

#[proc_macro_derive(EnvConfig, attributes(env_config))]
pub fn derive_env_config(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[derive(Default)]
struct FieldAttrs {
    key: Option<syn::LitStr>,
    env: Option<syn::LitStr>,
    default: Option<syn::Expr>,
    default_fn: Option<syn::Expr>,
    from_toml: Option<syn::Expr>,
    from_env: Option<syn::Expr>,
    to_toml: Option<syn::Expr>,
    allow_blank: bool,
}

impl FieldAttrs {
    fn parse(attrs: &[syn::Attribute]) -> syn::Result<Self> {
        let mut out = Self::default();
        for attr in attrs {
            if !attr.path().is_ident("env_config") {
                continue;
            }
            let metas = attr.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            )?;
            for meta in metas {
                match meta {
                    syn::Meta::Path(path) if path.is_ident("allow_blank") => {
                        out.allow_blank = true;
                    }
                    syn::Meta::NameValue(nv) => {
                        let Some(name) = nv.path.get_ident().map(ToString::to_string) else {
                            return Err(syn::Error::new_spanned(nv.path, "expected an identifier"));
                        };
                        match name.as_str() {
                            "key" => out.key = Some(expect_lit_str(&nv.value)?),
                            "env" => out.env = Some(expect_lit_str(&nv.value)?),
                            "default" => out.default = Some(nv.value),
                            "default_fn" => out.default_fn = Some(nv.value),
                            "from_toml" => out.from_toml = Some(nv.value),
                            "from_env" => out.from_env = Some(nv.value),
                            "to_toml" => out.to_toml = Some(nv.value),
                            other => {
                                return Err(syn::Error::new_spanned(
                                    nv.path,
                                    format!(
                                        "unknown env_config attribute `{other}`; expected one of key, env, default, default_fn, from_toml, from_env, to_toml, allow_blank"
                                    ),
                                ));
                            }
                        }
                    }
                    other => {
                        return Err(syn::Error::new_spanned(
                            other,
                            "expected `name = value` or the bare marker `allow_blank` inside env_config(...)",
                        ));
                    }
                }
            }
        }
        if let (Some(_), Some(default_fn)) = (&out.default, &out.default_fn) {
            return Err(syn::Error::new_spanned(
                default_fn,
                "`default` and `default_fn` are mutually exclusive",
            ));
        }
        Ok(out)
    }
}

fn expect_lit_str(expr: &syn::Expr) -> syn::Result<syn::LitStr> {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => Ok(s.clone()),
        other => Err(syn::Error::new_spanned(other, "expected a string literal")),
    }
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input,
            "EnvConfig can only be derived for structs with named fields",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &input,
            "EnvConfig requires named fields",
        ));
    };

    let mut field_idents = Vec::new();
    let mut field_stmts = Vec::new();
    let mut table_stmts = Vec::new();

    for field in &fields.named {
        let field_ident = field
            .ident
            .as_ref()
            .expect("Fields::Named guarantees an ident");
        let ty = &field.ty;
        let attrs = FieldAttrs::parse(&field.attrs)?;

        let field_name_lit = syn::LitStr::new(&field_ident.to_string(), field_ident.span());
        let key_lit = attrs.key.clone().unwrap_or_else(|| field_name_lit.clone());
        let env_expr = match &attrs.env {
            Some(lit) => quote! { ::core::option::Option::Some(#lit) },
            None => quote! { ::core::option::Option::None },
        };
        let allow_blank_lit = attrs.allow_blank;
        // Always wrapped in an explicit closure, never passed as a bare `fn`
        // item — `resolve_field`'s `impl Fn(&toml::Value) -> ...` parameter
        // is higher-ranked over the reference's lifetime, and rustc's
        // function-pointer-to-HRTB-closure coercion is unreliable once `T`
        // also needs to be inferred from a second, similarly-shaped
        // parameter (`from_env`) at the same call site — it silently pins a
        // bare `fn(&Value) -> _` item to one concrete lifetime and then
        // rejects it. A closure has no such inference wrinkle.
        let from_toml_expr = match &attrs.from_toml {
            Some(expr) => {
                quote! { |value: &::cli_engine::env_config::toml::Value| (#expr)(value) }
            }
            None => {
                quote! { |value: &::cli_engine::env_config::toml::Value| ::cli_engine::env_config::default_from_toml::<#ty>(value) }
            }
        };
        // Only require `T: FromStr` when the field is actually env-var
        // overridable (`env` given) or a custom `from_env` is provided —
        // `resolve_field` never calls `from_env` when `env_suffix` is `None`,
        // but it still needs *some* well-typed callable to pass in, so a
        // field with neither must not force a `FromStr` bound it doesn't need
        // (e.g. `Vec<String>`, which has no `FromStr`, used TOML-only).
        let from_env_expr = match (&attrs.from_env, &attrs.env) {
            (Some(expr), _) => quote! { |raw: &str| (#expr)(raw) },
            (None, Some(_)) => {
                quote! { |raw: &str| ::cli_engine::env_config::default_from_env::<#ty>(raw) }
            }
            (None, None) => quote! {
                |_raw: &str| -> ::core::result::Result<#ty, ::std::string::String> {
                    ::core::result::Result::Err(::std::string::String::new())
                }
            },
        };
        let default_arm = if let Some(expr) = &attrs.default {
            quote! { #expr }
        } else if let Some(expr) = &attrs.default_fn {
            quote! { (#expr)(sources) }
        } else {
            quote! {
                return ::core::result::Result::Err(
                    ::cli_engine::env_config::EnvConfigError::MissingField { field: #field_name_lit }
                )
            }
        };

        // Dual of `from_toml_expr`, for the `From<Self> for EnvTable`
        // direction below — same reasoning applies, an explicit closure
        // rather than a bare `fn` item.
        let to_toml_expr = match &attrs.to_toml {
            Some(expr) => quote! { (#expr)(value.#field_ident) },
            None => {
                quote! { ::core::convert::Into::<::cli_engine::env_config::toml::Value>::into(value.#field_ident) }
            }
        };

        field_idents.push(field_ident.clone());
        field_stmts.push(quote! {
            let #field_ident: #ty = match ::cli_engine::env_config::resolve_field::<#ty>(
                sources,
                #field_name_lit,
                #key_lit,
                #env_expr,
                #allow_blank_lit,
                #from_toml_expr,
                #from_env_expr,
            )? {
                ::core::option::Option::Some(value) => value,
                ::core::option::Option::None => #default_arm,
            };
        });
        table_stmts.push(quote! {
            table = table.with(#key_lit, #to_toml_expr);
        });
    }

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::cli_engine::env_config::EnvConfig for #ident #ty_generics #where_clause {
            fn assemble(
                sources: &::cli_engine::env_config::SourceChain<'_>,
            ) -> ::core::result::Result<Self, ::cli_engine::env_config::EnvConfigError> {
                #(#field_stmts)*
                ::core::result::Result::Ok(Self { #(#field_idents,)* })
            }
        }

        #[automatically_derived]
        impl #impl_generics ::core::convert::From<#ident #ty_generics> for ::cli_engine::environments::EnvTable #where_clause {
            /// Lets a compiled-in environment be registered as a plain struct
            /// value — `Environments::with_environment(name, MyConfig { .. })`
            /// — instead of a stringly-keyed `EnvTable`. Every field is
            /// written unconditionally (a struct literal has no "absent"
            /// state), using the same `key` each field's assembly
            /// instructions use.
            fn from(value: #ident #ty_generics) -> Self {
                let mut table = ::cli_engine::environments::EnvTable::new();
                #(#table_stmts)*
                table
            }
        }
    })
}
