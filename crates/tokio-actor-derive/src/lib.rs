//! Derive macros for `tokio-actor`.
//!
//! Do not depend on this crate directly: `tokio-actor` re-exports
//! `#[derive(Topology)]` under its default `derive` feature, and the generated
//! code refers to `tokio_actor` paths.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, parse_macro_input};

/// Derives a static actor topology from a named-field struct.
///
/// Each field is one actor in the graph. Every field type must implement
/// `tokio_actor::Actor`; any `MessageHandler` qualifies through the blanket
/// impl. For a struct named `Pipeline`, the derive generates:
///
/// * a `PipelineRefs` struct with one field per topology field, typed
///   `ActorRef<<FieldType as Actor>::Msg>`;
/// * `Pipeline::graph(wire)`, which builds the graph with a default
///   `GraphBuilder`;
/// * `Pipeline::graph_with(builder, wire)`, which accepts a preconfigured
///   `GraphBuilder` — graph name, mailbox capacity, shutdown timeouts, and
///   any extra actors registered by hand.
///
/// The `wire` closure receives `&PipelineRefs` before any actor value is
/// constructed, so actors can capture each other's refs even when the graph
/// is cyclic — no forward references or string lookups required:
///
/// ```
/// # use tokio_actor::{ActorContext, ActorRef, ActorResult, MessageHandler};
/// # struct FrontendMsg;
/// # struct ParserMsg;
/// # struct SinkMsg;
/// #
/// # #[derive(Clone)]
/// # struct Frontend {
/// #     parser: ActorRef<ParserMsg>,
/// # }
/// # impl MessageHandler for Frontend {
/// #     type Msg = FrontendMsg;
/// #     async fn handle(
/// #         &mut self,
/// #         _: FrontendMsg,
/// #         _: &ActorContext<FrontendMsg>,
/// #     ) -> ActorResult {
/// #         Ok(())
/// #     }
/// # }
/// #
/// # #[derive(Clone)]
/// # struct Parser {
/// #     frontend: ActorRef<FrontendMsg>,
/// #     sink: ActorRef<SinkMsg>,
/// # }
/// # impl MessageHandler for Parser {
/// #     type Msg = ParserMsg;
/// #     async fn handle(&mut self, _: ParserMsg, _: &ActorContext<ParserMsg>) -> ActorResult {
/// #         Ok(())
/// #     }
/// # }
/// #
/// # #[derive(Clone)]
/// # struct Sink;
/// # impl MessageHandler for Sink {
/// #     type Msg = SinkMsg;
/// #     async fn handle(&mut self, _: SinkMsg, _: &ActorContext<SinkMsg>) -> ActorResult {
/// #         Ok(())
/// #     }
/// # }
/// #
/// #[derive(tokio_actor::Topology)]
/// struct Pipeline {
///     frontend: Frontend,
///     parser: Parser,
///     sink: Sink,
/// }
///
/// # fn main() -> Result<(), tokio_actor::GraphBuildError> {
/// let graph = Pipeline::graph(|refs| Pipeline {
///     frontend: Frontend {
///         parser: refs.parser.clone(),
///     },
///     parser: Parser {
///         frontend: refs.frontend.clone(),
///         sink: refs.sink.clone(),
///     },
///     sink: Sink,
/// })?;
/// # let _ = graph;
/// # Ok(())
/// # }
/// ```
///
/// # Actor ids
///
/// Field names become actor ids verbatim. The id is what
/// `Graph::actor_ref::<M>("frontend")` looks up, and it appears in tracing
/// fields, metric labels, and supervisor child ids — renaming a field renames
/// all of those, but never affects type checking.
///
/// # Visibility
///
/// The refs struct and the generated `graph` / `graph_with` methods inherit
/// the topology struct's visibility; each refs field inherits the
/// corresponding topology field's visibility. A `pub` topology with `pub`
/// fields can therefore be wired from another module or crate.
///
/// # Compile-time guarantees
///
/// The derive rejects shapes it cannot wire, and the generated code keeps the
/// rest in the type system:
///
/// * enums, unions, tuple structs, and unit structs are rejected — actor ids
///   come from field names;
/// * generic structs are rejected;
/// * a struct with zero fields is rejected, because a graph must contain at
///   least one actor;
/// * a field whose type is not an actor fails to compile;
/// * wiring a ref whose message type does not match fails to compile;
/// * filling the same field twice is unrepresentable — the generated code
///   owns exactly one actor value per field.
///
/// # Errors
///
/// `graph` and `graph_with` return `GraphBuildError` for the runtime
/// configuration checks that remain, such as passing `graph_with` a builder
/// that already has an actor registered under the same id as a topology
/// field.
///
/// For dynamic graphs — actors created in a loop, or ids chosen at runtime —
/// use `GraphBuilder` directly instead of this derive.
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
