use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, parse_macro_input};

/// Derives a static actor topology from a named-field struct.
///
/// Each field type must implement `tokio_actor::Actor`. The generated
/// `<Topology>Refs` struct exposes one `ActorRef` per field, and the generated
/// `graph` / `graph_with` methods wire actors through a closure:
///
/// ```ignore
/// #[derive(tokio_actor::Topology)]
/// struct Pipeline {
///     frontend: Frontend,
///     parser: Parser,
///     sink: Sink,
/// }
///
/// let graph = Pipeline::graph(|refs| Pipeline {
///     frontend: Frontend::new(refs.parser.clone()),
///     parser: Parser::new(refs.frontend.clone(), refs.sink.clone()),
///     sink: Sink::new(),
/// })?;
/// ```
#[proc_macro_derive(Topology)]
pub fn derive_topology(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_topology(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_topology(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let topology = input.ident;
    let vis = input.vis;

    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            input.generics,
            "Topology cannot be derived for generic structs",
        ));
    }

    let fields = match input.data {
        Data::Struct(data) => match data.fields {
            Fields::Named(fields) => fields.named,
            Fields::Unnamed(fields) => {
                return Err(syn::Error::new_spanned(
                    fields,
                    "Topology can only be derived for structs with named fields",
                ));
            }
            Fields::Unit => {
                return Err(syn::Error::new_spanned(
                    &topology,
                    "Topology can only be derived for structs with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                &topology,
                "Topology can only be derived for structs with named fields",
            ));
        }
    };

    if fields.is_empty() {
        return Err(syn::Error::new_spanned(
            &topology,
            "Topology requires at least one actor field",
        ));
    }

    let refs = format_ident!("{topology}Refs");

    let field_idents: Vec<_> = fields
        .iter()
        .map(|field| field.ident.as_ref().expect("named fields"))
        .collect();
    let field_vis: Vec<_> = fields.iter().map(|field| &field.vis).collect();
    let field_types: Vec<_> = fields.iter().map(|field| &field.ty).collect();
    let field_names: Vec<_> = field_idents.iter().map(|ident| ident.to_string()).collect();
    let slot_idents: Vec<_> = field_idents
        .iter()
        .map(|ident| format_ident!("{ident}_slot"))
        .collect();

    Ok(quote! {
        #vis struct #refs {
            #(
                #field_vis #field_idents:
                    ::tokio_actor::ActorRef<<#field_types as ::tokio_actor::Actor>::Msg>,
            )*
        }

        impl #topology {
            #vis fn graph(
                wire: impl FnOnce(&#refs) -> #topology,
            ) -> ::core::result::Result<::tokio_actor::Graph, ::tokio_actor::GraphBuildError> {
                Self::graph_with(::tokio_actor::GraphBuilder::new(), wire)
            }

            #vis fn graph_with(
                mut builder: ::tokio_actor::GraphBuilder,
                wire: impl FnOnce(&#refs) -> #topology,
            ) -> ::core::result::Result<::tokio_actor::Graph, ::tokio_actor::GraphBuildError> {
                #(
                    let (#slot_idents, #field_idents) =
                        builder.slot::<<#field_types as ::tokio_actor::Actor>::Msg>(#field_names);
                )*

                let refs = #refs {
                    #(#field_idents,)*
                };
                let this = wire(&refs);

                #(
                    builder.define(#slot_idents, this.#field_idents);
                )*

                builder.build()
            }
        }
    })
}
