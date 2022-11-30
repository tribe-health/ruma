use std::ops::Not;

use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use syn::{
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    visit::Visit,
    DeriveInput, Field, Generics, Ident, ItemStruct, Lifetime, Token, Type,
};

use super::{
    attribute::{DeriveResponseMeta, ResponseMeta},
    ensure_feature_presence,
};
use crate::util::import_ruma_common;

mod incoming;
mod outgoing;

pub fn expand_response(attr: ResponseAttr, item: ItemStruct) -> TokenStream {
    let ruma_common = import_ruma_common();
    let ruma_macros = quote! { #ruma_common::exports::ruma_macros };

    let maybe_feature_error = ensure_feature_presence().map(syn::Error::to_compile_error);

    let error_ty = attr
        .0
        .iter()
        .find_map(|a| match a {
            DeriveResponseMeta::Error(ty) => Some(quote! { #ty }),
            _ => None,
        })
        .unwrap_or_else(|| quote! { #ruma_common::api::error::MatrixError });

    quote! {
        #maybe_feature_error

        #[derive(Clone, Debug, #ruma_macros::Response, #ruma_common::serde::_FakeDeriveSerde)]
        #[cfg_attr(not(feature = "unstable-exhaustive-types"), non_exhaustive)]
        #[ruma_api(error = #error_ty)]
        #item
    }
}

pub struct ResponseAttr(Punctuated<DeriveResponseMeta, Token![,]>);

impl Parse for ResponseAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        Punctuated::<DeriveResponseMeta, Token![,]>::parse_terminated(input).map(Self)
    }
}

pub fn expand_derive_response(input: DeriveInput) -> syn::Result<TokenStream> {
    let fields = match input.data {
        syn::Data::Struct(s) => s.fields,
        _ => panic!("This derive macro only works on structs"),
    };

    let fields = fields.into_iter().map(ResponseField::try_from).collect::<syn::Result<_>>()?;
    let mut manual_body_serde = false;
    let mut error_ty = None;
    for attr in input.attrs {
        if !attr.path.is_ident("ruma_api") {
            continue;
        }

        let metas =
            attr.parse_args_with(Punctuated::<DeriveResponseMeta, Token![,]>::parse_terminated)?;
        for meta in metas {
            match meta {
                DeriveResponseMeta::ManualBodySerde => manual_body_serde = true,
                DeriveResponseMeta::Error(t) => error_ty = Some(t),
            }
        }
    }

    let response = Response {
        ident: input.ident,
        generics: input.generics,
        fields,
        manual_body_serde,
        error_ty: error_ty.unwrap(),
    };

    response.check()?;
    Ok(response.expand_all())
}

struct Response {
    ident: Ident,
    generics: Generics,
    fields: Vec<ResponseField>,
    manual_body_serde: bool,
    error_ty: Type,
}

impl Response {
    /// Whether or not this request has any data in the HTTP body.
    fn has_body_fields(&self) -> bool {
        self.fields
            .iter()
            .any(|f| matches!(&f.kind, ResponseFieldKind::Body | &ResponseFieldKind::NewtypeBody))
    }

    /// Whether or not this request has a single newtype body field.
    fn has_newtype_body(&self) -> bool {
        self.fields.iter().any(|f| matches!(&f.kind, ResponseFieldKind::NewtypeBody))
    }

    /// Whether or not this request has a single raw body field.
    fn has_raw_body(&self) -> bool {
        self.fields.iter().any(|f| matches!(&f.kind, ResponseFieldKind::RawBody))
    }

    /// Whether or not this request has any data in the URL path.
    fn has_header_fields(&self) -> bool {
        self.fields.iter().any(|f| matches!(&f.kind, &ResponseFieldKind::Header(_)))
    }

    fn expand_all(&self) -> TokenStream {
        let ruma_common = import_ruma_common();
        let ruma_macros = quote! { #ruma_common::exports::ruma_macros };
        let serde = quote! { #ruma_common::exports::serde };

        let response_body_struct = self.has_body_fields().then(|| {
            let tuple = self.manual_body_serde.not().then(|| {
                // Only derive `Deserialize` if there are any fields,
                let derive_deserialize = (!self.fields.is_empty()).then(|| {
                    quote! {
                        #[cfg_attr(feature = "client", derive(#serde::Deserialize))]
                    }
                });

                // … otherwise `FromHttpBody` will be implemented directly.
                let from_http_body_impl = self.fields.is_empty().then(|| {
                    quote! {
                        #[automatically_derived]
                        impl<Error> #ruma_common::api::FromHttpBody<Error> for ResponseBody {
                            fn from_buf(_body: &[::std::primitive::u8]) -> Result<Self, Error> {
                                Ok(Self {})
                            }
                        }
                    }
                });

                let serde_derives = quote! {
                    #derive_deserialize
                    #[cfg_attr(feature = "server", derive(#serde::Serialize))]
                };

                (from_http_body_impl, serde_derives)
            });

            // FIXME: Use Option::unzip once MSRV >= 1.66
            let (from_http_body_impl, serde_derives) = match tuple {
                Some((a, b)) => (Some(a), Some(b)),
                None => (None, None),
            };

            let serde_attr = self.has_newtype_body().then(|| quote! { #[serde(transparent)] });
            let fields: Vec<_> =
                self.fields.iter().filter_map(ResponseField::as_body_field).collect();

            quote! {
                /// Data in the response body.
                #[doc(hidden)] // until type_alias_impl_trait works well enough
                #[non_exhaustive]
                #[cfg(any(feature = "client", feature = "server"))]
                #[derive(Debug, #ruma_macros::_FakeDeriveRumaApi, #ruma_macros::_FakeDeriveSerde)]
                #serde_derives
                #serde_attr
                pub struct ResponseBody { #(#fields),* }

                #from_http_body_impl
            }
        });

        let outgoing_response_impl = self.expand_outgoing(&ruma_common);
        let incoming_response_impl = self.expand_incoming(&self.error_ty, &ruma_common);

        quote! {
            #response_body_struct

            #outgoing_response_impl
            #incoming_response_impl
        }
    }

    pub fn check(&self) -> syn::Result<()> {
        // TODO: highlight problematic fields

        assert!(
            self.generics.params.is_empty() && self.generics.where_clause.is_none(),
            "This macro doesn't support generic types"
        );

        let newtype_body_fields = self.fields.iter().filter(|f| {
            matches!(&f.kind, ResponseFieldKind::NewtypeBody | ResponseFieldKind::RawBody)
        });

        let has_newtype_body_field = match newtype_body_fields.count() {
            0 => false,
            1 => true,
            _ => {
                return Err(syn::Error::new_spanned(
                    &self.ident,
                    "Can't have more than one newtype body field",
                ))
            }
        };

        let has_body_fields =
            self.fields.iter().any(|f| matches!(&f.kind, ResponseFieldKind::Body));
        if has_newtype_body_field && has_body_fields {
            return Err(syn::Error::new_spanned(
                &self.ident,
                "Can't have both a newtype body field and regular body fields",
            ));
        }

        Ok(())
    }
}

/// A field of the response struct.
struct ResponseField {
    inner: Field,
    kind: ResponseFieldKind,
}

/// The kind of a response field.
enum ResponseFieldKind {
    /// JSON data in the body of the response.
    Body,

    /// Data in an HTTP header.
    Header(Ident),

    /// A specific data type in the body of the response.
    NewtypeBody,

    /// Arbitrary bytes in the body of the response.
    RawBody,
}

impl ResponseField {
    /// Creates a new `ResponseField`.
    fn new(inner: Field, kind_attr: Option<ResponseMeta>) -> Self {
        let kind = match kind_attr {
            Some(ResponseMeta::NewtypeBody) => ResponseFieldKind::NewtypeBody,
            Some(ResponseMeta::RawBody) => ResponseFieldKind::RawBody,
            Some(ResponseMeta::Header(header)) => ResponseFieldKind::Header(header),
            None => ResponseFieldKind::Body,
        };

        Self { inner, kind }
    }

    /// Return the contained field if this response field is a body kind.
    fn as_body_field(&self) -> Option<&Field> {
        match &self.kind {
            ResponseFieldKind::Body | ResponseFieldKind::NewtypeBody => Some(&self.inner),
            _ => None,
        }
    }

    /// Return the contained field if this response field is a raw body kind.
    fn as_raw_body_field(&self) -> Option<&Field> {
        match &self.kind {
            ResponseFieldKind::RawBody => Some(&self.inner),
            _ => None,
        }
    }

    /// Return the contained field and HTTP header ident if this response field is a header kind.
    fn as_header_field(&self) -> Option<(&Field, &Ident)> {
        match &self.kind {
            ResponseFieldKind::Header(ident) => Some((&self.inner, ident)),
            _ => None,
        }
    }
}

impl TryFrom<Field> for ResponseField {
    type Error = syn::Error;

    fn try_from(mut field: Field) -> syn::Result<Self> {
        if has_lifetime(&field.ty) {
            return Err(syn::Error::new_spanned(
                field.ident,
                "Lifetimes on Response fields cannot be supported until GAT are stable",
            ));
        }

        let (mut api_attrs, attrs) =
            field.attrs.into_iter().partition::<Vec<_>, _>(|attr| attr.path.is_ident("ruma_api"));
        field.attrs = attrs;

        let kind_attr = match api_attrs.as_slice() {
            [] => None,
            [_] => Some(api_attrs.pop().unwrap().parse_args::<ResponseMeta>()?),
            _ => {
                return Err(syn::Error::new_spanned(
                    &api_attrs[1],
                    "multiple field kind attribute found, there can only be one",
                ));
            }
        };

        Ok(ResponseField::new(field, kind_attr))
    }
}

impl Parse for ResponseField {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        input.call(Field::parse_named)?.try_into()
    }
}

impl ToTokens for ResponseField {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        self.inner.to_tokens(tokens);
    }
}

fn has_lifetime(ty: &Type) -> bool {
    struct Visitor {
        found_lifetime: bool,
    }

    impl<'ast> Visit<'ast> for Visitor {
        fn visit_lifetime(&mut self, _lt: &'ast Lifetime) {
            self.found_lifetime = true;
        }
    }

    let mut vis = Visitor { found_lifetime: false };
    vis.visit_type(ty);
    vis.found_lifetime
}
