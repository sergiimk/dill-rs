extern crate proc_macro;

use darling::FromMeta;
use proc_macro::TokenStream;
use quote::{format_ident, quote, ToTokens};
use syn;

#[derive(FromMeta, Debug)]
struct ComponentOptions {
    #[darling(default)]
    scope: Option<syn::Path>,
}

#[proc_macro_attribute]
pub fn component(attr: TokenStream, item: TokenStream) -> TokenStream {
    let ast: syn::Item = syn::parse(item).unwrap();
    let vis: syn::Visibility = syn::parse(attr).unwrap();
    match ast {
        syn::Item::Struct(struct_ast) => component_from_struct(struct_ast),
        syn::Item::Impl(impl_ast) => component_from_impl(vis, impl_ast),
        _ => panic!("The #[component] macro can only be used on struct definiton or an impl block"),
    }
}

#[proc_macro_attribute]
pub fn scope(_args: TokenStream, item: TokenStream) -> TokenStream {
    item
}

fn component_from_struct(ast: syn::ItemStruct) -> TokenStream {
    let impl_name = &ast.ident;
    let impl_type = syn::parse2(quote! { #impl_name }).unwrap();

    let args: Vec<_> = ast
        .fields
        .iter()
        .map(|f| (f.ident.clone().unwrap(), f.ty.clone()))
        .collect();

    let scope_type =
        get_scope(&ast.attrs).unwrap_or_else(|| syn::parse_str("::dill::Transient").unwrap());

    let mut gen: TokenStream = quote! { #ast }.into();
    let builder: TokenStream = implement_builder(&ast.vis, &impl_type, scope_type, args, false);

    gen.extend(builder.into_iter());
    gen
}

fn component_from_impl(vis: syn::Visibility, ast: syn::ItemImpl) -> TokenStream {
    let impl_type = &ast.self_ty;
    let new = get_new(&ast.items).expect(
        "When using #[component] macro on the impl block it's expected to contain a new() function. \
        Otherwise use #[derive(Builder)] on the struct."
    );

    let args: Vec<_> = new
        .sig
        .inputs
        .iter()
        .map(|arg| match arg {
            syn::FnArg::Typed(targ) => targ,
            _ => panic!("Unexpected argument in new() function"),
        })
        .map(|arg| {
            (
                match arg.pat.as_ref() {
                    syn::Pat::Ident(ident) => ident.ident.clone(),
                    _ => panic!("Unexpected format of arguments in new() function"),
                },
                arg.ty.as_ref().clone(),
            )
        })
        .collect();

    let scope_type =
        get_scope(&ast.attrs).unwrap_or_else(|| syn::parse_str("::dill::Transient").unwrap());

    let mut gen: TokenStream = quote! { #ast }.into();
    let builder: TokenStream = implement_builder(&vis, impl_type, scope_type, args, true);

    gen.extend(builder.into_iter());
    gen
}

fn implement_builder(
    impl_vis: &syn::Visibility,
    impl_type: &syn::Type,
    scope_type: syn::Path,
    args: Vec<(syn::Ident, syn::Type)>,
    has_new: bool,
) -> TokenStream {
    let builder_name = format_ident!("{}Builder", quote! { #impl_type }.to_string());

    let arg_name: Vec<_> = args.iter().map(|(name, _)| name).collect();
    let arg_impls: Vec<_> = args
        .iter()
        .map(|(name, typ)| implement_arg(name, typ, &builder_name))
        .collect();

    // Unzip
    let mut arg_override_fn_field = Vec::new();
    let mut arg_override_fn_field_ctor = Vec::new();
    let mut arg_override_setters = Vec::new();
    let mut arg_prepare_dependency = Vec::new();
    let mut arg_provide_dependency = Vec::new();
    for (
        override_fn_field,
        override_fn_field_ctor,
        override_setters,
        prepare_dependency,
        provide_dependency,
    ) in arg_impls
    {
        arg_override_fn_field.push(override_fn_field);
        arg_override_fn_field_ctor.push(override_fn_field_ctor);
        arg_override_setters.push(override_setters);
        arg_prepare_dependency.push(prepare_dependency);
        arg_provide_dependency.push(provide_dependency);
    }

    let ctor = if !has_new {
        quote! {
            #impl_type {
                #( #arg_name: #arg_provide_dependency, )*
            }
        }
    } else {
        quote! {
            #impl_type::new(#( #arg_provide_dependency, )*)
        }
    };

    let gen = quote! {
        impl ::dill::BuilderLike for #impl_type {
            type Builder = #builder_name;
            fn register(cat: &mut ::dill::CatalogBuilder) {
                cat.add_builder(Self::builder());
            }
            fn builder() -> Self::Builder {
                #builder_name::new()
            }
        }

        #impl_vis struct #builder_name {
            scope: #scope_type,
            #(
                #arg_override_fn_field
            )*
        }

        impl #builder_name {
            pub fn new() -> Self {
                Self {
                    scope: #scope_type::new(),
                    #(
                        #arg_override_fn_field_ctor
                    )*
                }
            }

            #( #arg_override_setters )*

            fn build(&self, cat: &::dill::Catalog) -> Result<#impl_type, ::dill::InjectionError> {
                #( #arg_prepare_dependency )*
                Ok(#ctor)
            }
        }

        impl ::dill::Builder for #builder_name {
            fn instance_type_id(&self) -> std::any::TypeId {
                std::any::TypeId::of::<#impl_type>()
            }

            fn instance_type_name(&self) -> &'static str {
                std::any::type_name::<#impl_type>()
            }

            fn get(&self, cat: &::dill::Catalog) -> Result<std::sync::Arc<dyn std::any::Any + Send + Sync>, ::dill::InjectionError> {
                Ok(::dill::TypedBuilder::get(self, cat)?)
            }
        }

        impl ::dill::TypedBuilder<#impl_type> for #builder_name {
            fn get(&self, cat: &::dill::Catalog) -> Result<std::sync::Arc<#impl_type>, ::dill::InjectionError> {
                use dill::Scope;

                if let Some(inst) = self.scope.get() {
                    return Ok(inst.downcast().unwrap());
                }

                let inst = std::sync::Arc::new(self.build(cat)?);

                self.scope.set(inst.clone());
                Ok(inst)
            }
        }
    };

    gen.into()
}

fn implement_arg(
    name: &syn::Ident,
    typ: &syn::Type,
    builder: &syn::Ident,
) -> (
    proc_macro2::TokenStream,
    proc_macro2::TokenStream,
    proc_macro2::TokenStream,
    proc_macro2::TokenStream,
    proc_macro2::TokenStream,
) {
    let override_fn_name = format_ident!("arg_{}_fn", name);

    let override_fn_field = if is_reference(typ) {
        proc_macro2::TokenStream::new()
    } else {
        quote! {
            #override_fn_name: Option<Box<dyn Fn(&::dill::Catalog) -> Result<#typ, ::dill::InjectionError> + Send + Sync>>,
        }
    };

    let override_fn_field_ctor = if is_reference(typ) {
        proc_macro2::TokenStream::new()
    } else {
        quote! { #override_fn_name: None, }
    };

    let override_setters = if is_reference(typ) {
        proc_macro2::TokenStream::new()
    } else {
        let setter_val_name = format_ident!("with_{}", name);
        let setter_fn_name = format_ident!("with_{}_fn", name);
        quote! {
            pub fn #setter_val_name(mut self, val: #typ) -> #builder {
                self.#override_fn_name = Some(Box::new(move |_| Ok(val.clone())));
                self
            }

            pub fn #setter_fn_name(
                mut self,
                fun: impl Fn(&::dill::Catalog) -> Result<#typ, ::dill::InjectionError> + 'static + Send + Sync
            ) -> #builder {
                self.#override_fn_name = Some(Box::new(fun));
                self
            }
        }
    };

    let from_catalog = if is_reference(typ) {
        let stripped = strip_reference(typ);
        quote! { cat.get::<OneOf<#stripped>>()? }
    } else if is_smart_ptr(typ) {
        let stripped = strip_smart_ptr(typ);
        quote! { cat.get::<OneOf<#stripped>>()? }
    } else {
        quote! { cat.get::<OneOf<#typ>>().map(|v| v.as_ref().clone())? }
    };

    let prepare_dependency = if is_reference(typ) {
        quote! { let #name = #from_catalog; }
    } else {
        quote! {
            let #name = match self.#override_fn_name {
                Some(ref fun) => fun(cat)?,
                _ => #from_catalog,
            };
        }
    };

    let provide_dependency = if is_reference(typ) {
        quote! { #name.as_ref() }
    } else {
        quote! { #name }
    };

    (
        override_fn_field,
        override_fn_field_ctor,
        override_setters,
        prepare_dependency,
        provide_dependency,
    )
}

/// Searches for `#[scope(X)]` attribute and returns `X`
fn get_scope(attrs: &Vec<syn::Attribute>) -> Option<syn::Path> {
    attrs
        .iter()
        .filter_map(|a| a.parse_meta().ok())
        .filter_map(|m| match m {
            syn::Meta::List(ml) => Some(ml),
            _ => None,
        })
        .filter(|ml| ml.path.is_ident("scope"))
        .next()
        .and_then(|ml| match ml.nested.into_iter().next() {
            Some(syn::NestedMeta::Meta(syn::Meta::Path(p))) => Some(p),
            _ => panic!("Invalid scope attribute"),
        })
}

/// Searches `impl` block for `new()` method
fn get_new(impl_items: &Vec<syn::ImplItem>) -> Option<&syn::ImplItemMethod> {
    impl_items
        .iter()
        .filter_map(|i| match i {
            syn::ImplItem::Method(m) => Some(m),
            _ => None,
        })
        .filter(|m| m.sig.ident == "new")
        .next()
}

fn is_reference(typ: &syn::Type) -> bool {
    match typ {
        syn::Type::Reference(_) => true,
        _ => false,
    }
}

fn strip_reference(typ: &syn::Type) -> syn::Type {
    match typ {
        syn::Type::Reference(r) => r.elem.as_ref().clone(),
        _ => typ.clone(),
    }
}

fn is_smart_ptr(typ: &syn::Type) -> bool {
    match typ {
        syn::Type::Path(typepath) if typepath.qself.is_none() => {
            match typepath.path.segments.first() {
                Some(seg) if seg.ident.to_string() == "Arc" => true,
                _ => false,
            }
        }
        _ => false,
    }
}

fn strip_smart_ptr(typ: &syn::Type) -> syn::Type {
    match typ {
        syn::Type::Path(typepath) if typepath.qself.is_none() => {
            match typepath.path.segments.first() {
                Some(seg) if seg.ident.to_string() == "Arc" => match seg.arguments {
                    syn::PathArguments::AngleBracketed(ref args) => {
                        syn::parse2(args.args.to_token_stream()).unwrap()
                    }
                    _ => typ.clone(),
                },
                _ => typ.clone(),
            }
        }
        _ => typ.clone(),
    }
}
