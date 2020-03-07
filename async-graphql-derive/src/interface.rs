use crate::args;
use crate::args::{InterfaceField, InterfaceFieldArgument};
use crate::output_type::OutputType;
use crate::utils::{build_value_repr, get_crate_name};
use proc_macro::TokenStream;
use proc_macro2::{Ident, Span};
use quote::quote;
use syn::{Data, DeriveInput, Error, Fields, Result, Type};

// todo: Context params

pub fn generate(interface_args: &args::Interface, input: &DeriveInput) -> Result<TokenStream> {
    let crate_name = get_crate_name(interface_args.internal);
    let ident = &input.ident;
    let generics = &input.generics;
    let attrs = &input.attrs;
    let vis = &input.vis;
    let s = match &input.data {
        Data::Struct(s) => s,
        _ => return Err(Error::new_spanned(input, "It should be a struct.")),
    };
    let fields = match &s.fields {
        Fields::Unnamed(fields) => fields,
        _ => return Err(Error::new_spanned(input, "All fields must be unnamed.")),
    };
    let mut enum_names = Vec::new();
    let mut enum_items = Vec::new();
    let mut type_into_impls = Vec::new();
    let gql_typename = interface_args
        .name
        .clone()
        .unwrap_or_else(|| ident.to_string());
    let desc = interface_args
        .desc
        .as_ref()
        .map(|s| quote! {Some(#s)})
        .unwrap_or_else(|| quote! {None});
    let mut registry_types = Vec::new();
    let mut possible_types = Vec::new();
    let mut inline_fragment_resolvers = Vec::new();

    for field in &fields.unnamed {
        if let Type::Path(p) = &field.ty {
            let enum_name = &p.path.segments.last().unwrap().ident;
            enum_names.push(enum_name);
            enum_items.push(quote! { #enum_name(#p) });
            type_into_impls.push(quote! {
                impl #generics From<#p> for #ident #generics {
                    fn from(obj: #p) -> Self {
                        #ident::#enum_name(obj)
                    }
                }
            });
            registry_types.push(quote! {
                <#p as async_graphql::GQLType>::create_type_info(registry);
                registry.add_implements(&<#p as GQLType>::type_name(), #gql_typename);
            });
            possible_types.push(quote! {
                <#p as async_graphql::GQLType>::type_name().to_string()
            });
            inline_fragment_resolvers.push(quote! {
                if name == <#p as async_graphql::GQLType>::type_name() {
                    if let #ident::#enum_name(obj) = self {
                        #crate_name::do_resolve(ctx, obj, result).await?;
                    }
                    return Ok(());
                }
            });
        } else {
            return Err(Error::new_spanned(field, "Invalid type"));
        }
    }

    let mut methods = Vec::new();
    let mut schema_fields = Vec::new();
    let mut resolvers = Vec::new();

    for InterfaceField {
        name,
        method: method_name,
        desc,
        ty,
        args,
        deprecation,
    } in &interface_args.fields
    {
        let method_name = Ident::new(
            method_name.as_ref().unwrap_or_else(|| &name),
            Span::call_site(),
        );
        let mut calls = Vec::new();
        let mut use_params = Vec::new();
        let mut decl_params = Vec::new();
        let mut get_params = Vec::new();
        let mut schema_args = Vec::new();

        for InterfaceFieldArgument {
            name,
            desc,
            ty,
            default,
        } in args
        {
            let ident = Ident::new(name, Span::call_site());
            decl_params.push(quote! { #ident: #ty });
            use_params.push(ident.clone());

            let param_default = match &default {
                Some(default) => {
                    let repr = build_value_repr(&crate_name, &default);
                    quote! {|| #repr }
                }
                None => quote! { || #crate_name::Value::Null },
            };
            get_params.push(quote! {
                let #ident: #ty = ctx_field.param_value(#name, #param_default)?;
            });

            let desc = desc
                .as_ref()
                .map(|s| quote! {Some(#s)})
                .unwrap_or_else(|| quote! {None});
            let schema_default = default
                .as_ref()
                .map(|v| {
                    let s = v.to_string();
                    quote! {Some(#s)}
                })
                .unwrap_or_else(|| quote! {None});
            schema_args.push(quote! {
                #crate_name::registry::InputValue {
                    name: #name,
                    description: #desc,
                    ty: <#ty as #crate_name::GQLType>::create_type_info(registry),
                    default_value: #schema_default,
                }
            });
        }

        for enum_name in &enum_names {
            calls.push(quote! {
                #ident::#enum_name(obj) => obj.#method_name(#(#use_params),*).await
            });
        }

        methods.push(quote! {
            async fn #method_name(&self, #(#decl_params),*) -> #ty {
                match self {
                    #(#calls,)*
                }
            }
        });

        let desc = desc
            .as_ref()
            .map(|s| quote! {Some(#s)})
            .unwrap_or_else(|| quote! {None});
        let deprecation = deprecation
            .as_ref()
            .map(|s| quote! {Some(#s)})
            .unwrap_or_else(|| quote! {None});

        let ty = OutputType::parse(ty)?;
        let value_ty = ty.value_type();

        schema_fields.push(quote! {
            #crate_name::registry::Field {
                name: #name,
                description: #desc,
                args: vec![#(#schema_args),*],
                ty: <#value_ty as #crate_name::GQLType>::create_type_info(registry),
                deprecation: #deprecation,
            }
        });

        let resolve_obj = match &ty {
            OutputType::Value(_) => quote! {
                self.#method_name(#(#use_params),*).await
            },
            OutputType::Result(_, _) => {
                quote! {
                    self.#method_name(#(#use_params),*).await.
                        map_err(|err| err.with_position(field.position))?
                }
            }
        };

        resolvers.push(quote! {
            if field.name.as_str() == #name {
                #(#get_params)*
                let ctx_obj = ctx.with_item(&field.selection_set);
                return #crate_name::GQLOutputValue::resolve(&#resolve_obj, &ctx_obj).await.
                    map_err(|err| err.with_position(field.position).into());
            }
        });
    }

    let expanded = quote! {
        #(#attrs)*
        #vis enum #ident #generics { #(#enum_items),* }

        #(#type_into_impls)*

        impl #generics #ident #generics {
            #(#methods)*
        }

        impl #generics #crate_name::GQLType for #ident #generics {
            fn type_name() -> Cow<'static, str> {
                Cow::Borrowed(#gql_typename)
            }

            fn create_type_info(registry: &mut #crate_name::registry::Registry) -> String {
                registry.create_type::<Self, _>(|registry| {
                    #(#registry_types)*

                    async_graphql::registry::Type::Interface {
                        name: #gql_typename,
                        description: #desc,
                        fields: vec![#(#schema_fields),*],
                        possible_types: vec![#(#possible_types),*],
                    }
                })
            }
        }

        #[#crate_name::async_trait::async_trait]
        impl #generics #crate_name::GQLObject for #ident #generics {
            async fn resolve_field(&self, ctx: &#crate_name::Context<'_>, field: &#crate_name::graphql_parser::query::Field) -> #crate_name::Result<#crate_name::serde_json::Value> {
                use #crate_name::ErrorWithPosition;

                #(#resolvers)*

                anyhow::bail!(#crate_name::QueryError::FieldNotFound {
                    field_name: field.name.clone(),
                    object: #gql_typename.to_string(),
                }
                .with_position(field.position));
            }

            async fn resolve_inline_fragment(&self, name: &str, ctx: &#crate_name::ContextSelectionSet<'_>, result: &mut #crate_name::serde_json::Map<String, serde_json::Value>) -> #crate_name::Result<()> {
                #(#inline_fragment_resolvers)*
                anyhow::bail!(#crate_name::QueryError::UnrecognizedInlineFragment {
                    object: #gql_typename.to_string(),
                    name: name.to_string(),
                });
            }
        }
    };
    Ok(expanded.into())
}
