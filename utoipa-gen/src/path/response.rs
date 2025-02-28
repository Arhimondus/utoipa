use std::{borrow::Cow, mem};

use proc_macro2::{Ident, Span, TokenStream as TokenStream2};
use proc_macro_error::{abort, ResultExt};
use quote::{quote, quote_spanned, ToTokens};
use syn::{
    parenthesized,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    spanned::Spanned,
    token::Comma,
    Attribute, Data, Error, ExprPath, Field, Fields, Generics, LitInt, LitStr, Path, Token, Type,
    TypePath, Variant,
};

use crate::{
    component::{
        schema::{EnumSchema, NamedStructSchema},
        TypeTree,
    },
    doc_comment::CommentAttributes,
    parse_utils, AnyValue, Array,
};

use super::{
    example::Example, media_type::MediaTypeSchema, status::STATUS_CODES, InlineType, PathType,
    PathTypeTree,
};

pub mod derive;

enum DeriveResponseType<'r> {
    Unnamed(Type, &'r [Attribute]),
    Named(Type, &'r Punctuated<Field, Comma>),
    Unit,
    Enum(Type, &'r Punctuated<Variant, Comma>),
}

pub struct DeriveResponse {
    pub attributes: Vec<Attribute>,
    pub data: Data,
    pub generics: Generics,
    pub ident: Ident,
}

impl DeriveResponse {
    fn get_type(&self) -> DeriveResponseType {
        let get_type = || {
            let path = Path::from(self.ident.clone());
            let type_path = TypePath { path, qself: None };
            Type::Path(type_path)
        };
        match &self.data {
            Data::Struct(value) => {
                match &value.fields {
                    Fields::Named(named) => DeriveResponseType::Named(get_type(), &named.named),
                    Fields::Unit => DeriveResponseType::Unit,
                    Fields::Unnamed(unnamed) => {
                        if unnamed.unnamed.len() <= 1 {
                            unnamed
                                .unnamed
                                .iter()
                                .next()
                                .map(|field| DeriveResponseType::Unnamed(field.ty.clone(), field.attrs.as_slice())).unwrap_or_else(|| abort!(unnamed.span(), "Unnamed struct used for `ToResponse` must have one argument"))
                        } else {
                            abort!(
                                unnamed.span(),
                                "Unnamed struct with tuple value is unsupported in `ToResponse`"
                            );
                        }
                    }
                }
            }
            Data::Enum(variants) => DeriveResponseType::Enum(get_type(), &variants.variants),
            _ => abort!(self.ident, "Union type is not supported with `ToResponse`"),
        }
    }

    fn parse_derive_response_value(
        &self,
        attributes: &[Attribute],
    ) -> Option<DeriveToResponseValue> {
        attributes
            .iter()
            .filter(|attribute| attribute.path.get_ident().unwrap() == "response")
            .map(|attribute| {
                attribute
                    .parse_args::<DeriveToResponseValue>()
                    .unwrap_or_abort()
            })
            .reduce(|acc, item| acc.merge_from(item))
    }

    fn create_response<'r>(
        &'r self,
        description: String,
        ty: Option<PathType<'r>>,
        content: Punctuated<Content<'r>, Comma>,
    ) -> ResponseTuple {
        let response_value = self.parse_derive_response_value(self.attributes.as_slice());
        if let Some(response_value) = response_value {
            if (!content.is_empty() && response_value.example.is_some())
                || (!content.is_empty() && response_value.examples.is_some())
            {
                let ident = response_value
                    .example
                    .as_ref()
                    .map(|(_, ident)| ident)
                    .or_else(|| response_value.examples.as_ref().map(|(_, ident)| ident))
                    .expect("Expected `example` or `examples` to be present");
                abort! {
                    ident,
                    "Enum with `#[content]` attribute in variant cannot have enum level `example` or `examples` defined";
                    help = "Try defining `{}` on the enum variant", ident.to_string(),
                }
            }
            let value = ResponseValue {
                description: if response_value.description.is_empty() && !description.is_empty() {
                    description
                } else {
                    response_value.description
                },
                headers: response_value.headers,
                example: response_value.example.map(|(example, _)| example),
                examples: response_value.examples.map(|(examples, _)| examples),
                content_type: response_value.content_type,
                response_type: if content.is_empty() { ty } else { None },
                content,
            };

            ResponseTuple {
                inner: Some(ResponseTupleInner::Value(value)),
                ..Default::default()
            }
        } else {
            let value = ResponseValue {
                response_type: if content.is_empty() { ty } else { None },
                content,
                description,
                ..Default::default()
            };
            ResponseTuple {
                inner: Some(ResponseTupleInner::Value(value)),
                ..Default::default()
            }
        }
    }
}

impl ToTokens for DeriveResponse {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        // construct default type for the response
        let derive_response_type = self.get_type();
        let description =
            CommentAttributes::from_attributes(&self.attributes).as_formatted_string();

        let response = match derive_response_type {
            DeriveResponseType::Unnamed(ty, attributes) => {
                let is_inline = attributes
                    .iter()
                    .any(|attribute| attribute.path.get_ident().unwrap() == "to_schema");
                self.create_response(
                    description,
                    Some(PathType::MediaType(InlineType {
                        ty: Cow::Owned(ty),
                        is_inline,
                    })),
                    Punctuated::new(),
                )
            }
            DeriveResponseType::Named(ty, fields) => {
                let inline_schema = NamedStructSchema {
                    alias: None,
                    fields,
                    features: None,
                    generics: None,
                    attributes: self.attributes.as_slice(),
                    struct_name: Cow::Owned(self.ident.to_string()),
                    rename_all: None,
                };
                self.create_response(
                    description,
                    Some(PathType::InlineSchema(inline_schema.to_token_stream(), ty)),
                    Punctuated::new(),
                )
            }
            DeriveResponseType::Unit => self.create_response(description, None, Punctuated::new()),
            DeriveResponseType::Enum(ty, variants) => {
                let variants_content = variants
                    .iter()
                    .map(|variant| {
                        let variant_derive_response_value =
                            self.parse_derive_response_value(variant.attrs.as_slice());
                        let field = variant.fields.iter().next();

                        let content_type = field.and_then(|field| {
                            field
                                .attrs
                                .iter()
                                .find(|attribute| attribute.path.get_ident().unwrap() == "content")
                                .map(|attribute| {
                                    attribute
                                        .parse_args_with(|input: ParseStream| {
                                            input.parse::<LitStr>()
                                        })
                                        .unwrap_or_abort()
                                })
                                .map(|content| content.value())
                        });
                        let is_inline = field
                            .map(|field| {
                                field.attrs.iter().any(|attribute| {
                                    attribute.path.get_ident().unwrap() == "to_schema"
                                })
                            })
                            .unwrap_or(false);

                        (
                            field.map(|field| field.ty.clone()).zip(content_type),
                            variant_derive_response_value,
                            is_inline,
                        )
                    })
                    .filter_map(|(field, mut variant_derive, is_inline)| {
                        let (example, examples) = if let Some(variant_derive) = &mut variant_derive
                        {
                            (
                                mem::take(&mut variant_derive.example),
                                mem::take(&mut variant_derive.examples),
                            )
                        } else {
                            (None, None)
                        };

                        field.map(|(ty, content_type)| {
                            Content(
                                content_type,
                                PathType::MediaType(InlineType {
                                    ty: Cow::Owned(ty),
                                    is_inline,
                                }),
                                example.map(|(example, _)| example),
                                examples.map(|(examples, _)| examples),
                            )
                        })
                    });

                let content: Punctuated<Content, Comma> = Punctuated::from_iter(variants_content);

                self.create_response(
                    description,
                    // enums with #[content] attribute uses schema reference
                    if content.len() > 1 {
                        None
                    } else {
                        let inline_schema = EnumSchema {
                            variants,
                            attributes: self.attributes.as_slice(),
                            enum_name: Cow::Owned(self.ident.to_string()),
                        };

                        Some(PathType::InlineSchema(
                            inline_schema.into_token_stream(),
                            ty,
                        ))
                    },
                    content,
                )
            }
        };

        let ident = &self.ident;
        let name = &*self.ident.to_string();

        let (impl_generics, ty_generics, where_clause) = self.generics.split_for_impl();
        tokens.extend(quote! {
            impl #impl_generics utoipa::ToResponse for #ident #ty_generics #where_clause {
                fn response() -> (String, utoipa::openapi::RefOr<utoipa::openapi::response::Response>) {
                    (#name.to_string(), #response.into())
                }
            }
        });
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
pub enum Response<'r> {
    /// A type that implements `utoipa::IntoResponses`.
    IntoResponses(ExprPath),
    /// The tuple definition of a response.
    Tuple(ResponseTuple<'r>),
}

impl Parse for Response<'_> {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.fork().parse::<ExprPath>().is_ok() {
            Ok(Self::IntoResponses(input.parse()?))
        } else {
            let response;
            parenthesized!(response in input);
            Ok(Self::Tuple(response.parse()?))
        }
    }
}

/// Parsed representation of response attributes from `#[utoipa::path]` attribute.
#[derive(Default)]
#[cfg_attr(feature = "debug", derive(Debug))]
pub struct ResponseTuple<'r> {
    status_code: ResponseStatus,
    inner: Option<ResponseTupleInner<'r>>,
}

const RESPONSE_INCOMPATIBLE_ATTRIBUTES_MSG: &str =
    "The `response` attribute may only be used in conjunction with the `status` attribute";

impl<'r> ResponseTuple<'r> {
    // This will error if the `response` attribute has already been set
    fn as_value(&mut self, span: Span) -> syn::Result<&mut ResponseValue<'r>> {
        if self.inner.is_none() {
            self.inner = Some(ResponseTupleInner::Value(ResponseValue::default()));
        }
        if let ResponseTupleInner::Value(val) = self.inner.as_mut().unwrap() {
            Ok(val)
        } else {
            Err(Error::new(span, RESPONSE_INCOMPATIBLE_ATTRIBUTES_MSG))
        }
    }

    // Use with the `response` attribute, this will fail if an incompatible attribute has already been set
    fn set_ref_type(&mut self, span: Span, ty: InlineType<'r>) -> syn::Result<()> {
        match &mut self.inner {
            None => self.inner = Some(ResponseTupleInner::Ref(ty)),
            Some(ResponseTupleInner::Ref(r)) => *r = ty,
            Some(ResponseTupleInner::Value(_)) => {
                return Err(Error::new(span, RESPONSE_INCOMPATIBLE_ATTRIBUTES_MSG))
            }
        }
        Ok(())
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
enum ResponseTupleInner<'r> {
    Value(ResponseValue<'r>),
    Ref(InlineType<'r>),
}

impl Parse for ResponseTuple<'_> {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        const EXPECTED_ATTRIBUTE_MESSAGE: &str = "unexpected attribute, expected any of: status, description, body, content_type, headers, example, examples, response";

        let mut response = ResponseTuple::default();

        while !input.is_empty() {
            let ident = input.parse::<Ident>().map_err(|error| {
                Error::new(
                    error.span(),
                    format!("{}, {}", EXPECTED_ATTRIBUTE_MESSAGE, error),
                )
            })?;
            let attribute_name = &*ident.to_string();

            match attribute_name {
                "status" => {
                    response.status_code =
                        parse_utils::parse_next(input, || input.parse::<ResponseStatus>())?;
                }
                "description" => {
                    response.as_value(input.span())?.description = parse::description(input)?;
                }
                "body" => {
                    response.as_value(input.span())?.response_type =
                        Some(parse_utils::parse_next(input, || input.parse())?);
                }
                "content_type" => {
                    response.as_value(input.span())?.content_type =
                        Some(parse::content_type(input)?);
                }
                "headers" => {
                    response.as_value(input.span())?.headers = parse::headers(input)?;
                }
                "example" => {
                    response.as_value(input.span())?.example = Some(parse::example(input)?);
                }
                "examples" => {
                    response.as_value(input.span())?.examples = Some(parse::examples(input)?);
                }
                "content" => {
                    response.as_value(input.span())?.content =
                        parse_utils::parse_punctuated_within_parenthesis(input)?;
                }
                "response" => {
                    response.set_ref_type(
                        input.span(),
                        parse_utils::parse_next(input, || input.parse())?,
                    )?;
                }
                _ => return Err(Error::new(ident.span(), EXPECTED_ATTRIBUTE_MESSAGE)),
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        if response.inner.is_none() {
            response.inner = Some(ResponseTupleInner::Value(ResponseValue::default()))
        }

        Ok(response)
    }
}

#[derive(Default)]
#[cfg_attr(feature = "debug", derive(Debug))]
pub struct ResponseValue<'r> {
    description: String,
    response_type: Option<PathType<'r>>,
    content_type: Option<Vec<String>>,
    headers: Vec<Header>,
    example: Option<AnyValue>,
    examples: Option<Punctuated<Example, Comma>>,
    content: Punctuated<Content<'r>, Comma>,
}

impl ToTokens for ResponseTuple<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        match self.inner.as_ref().unwrap() {
            ResponseTupleInner::Ref(res) => {
                let path = &res.ty;
                if res.is_inline {
                    tokens.extend(quote_spanned! {path.span()=>
                        <#path as utoipa::ToResponse>::response().1
                    });
                } else {
                    tokens.extend(quote! {
                        utoipa::openapi::Ref::from_response_name(<#path as utoipa::ToResponse>::response().0)
                    });
                }
            }
            ResponseTupleInner::Value(val) => {
                let description = &val.description;
                tokens.extend(quote! {
                    utoipa::openapi::ResponseBuilder::new().description(#description)
                });

                let create_content = |path_type: &PathType,
                                      example: &Option<AnyValue>,
                                      examples: &Option<Punctuated<Example, Comma>>|
                 -> TokenStream2 {
                    let content_schema = match path_type {
                        PathType::Ref(ref_type) => quote! {
                            utoipa::openapi::schema::Ref::new(#ref_type)
                        }
                        .to_token_stream(),
                        PathType::MediaType(ref path_type) => {
                            let type_tree = path_type.as_type_tree();
                            MediaTypeSchema {
                                type_tree: &type_tree,
                                is_inline: path_type.is_inline,
                            }
                            .to_token_stream()
                        }
                        PathType::InlineSchema(schema, _) => schema.to_token_stream(),
                    };

                    let mut content =
                        quote! { utoipa::openapi::ContentBuilder::new().schema(#content_schema) };

                    if let Some(ref example) = example {
                        content.extend(quote! {
                            .example(Some(#example))
                        })
                    }
                    if let Some(ref examples) = examples {
                        let examples = examples
                            .iter()
                            .map(|example| {
                                let name = &example.name;
                                quote!((#name, #example))
                            })
                            .collect::<Array<TokenStream2>>();
                        content.extend(quote!(
                            .examples_from_iter(#examples)
                        ))
                    }

                    quote! {
                        #content.build()
                    }
                };

                if let Some(response_type) = &val.response_type {
                    let content = create_content(response_type, &val.example, &val.examples);

                    if let Some(content_types) = val.content_type.as_ref() {
                        content_types.iter().for_each(|content_type| {
                            tokens.extend(quote! {
                                .content(#content_type, #content)
                            })
                        })
                    } else {
                        match response_type {
                            PathType::Ref(_) => {
                                tokens.extend(quote! {
                                    .content("application/json", #content)
                                });
                            }
                            PathType::MediaType(path_type) => {
                                let type_tree = path_type.as_type_tree();
                                let default_type = type_tree.get_default_content_type();
                                tokens.extend(quote! {
                                    .content(#default_type, #content)
                                })
                            }
                            PathType::InlineSchema(_, ty) => {
                                let type_tree = TypeTree::from_type(ty);
                                let default_type = type_tree.get_default_content_type();
                                tokens.extend(quote! {
                                    .content(#default_type, #content)
                                })
                            }
                        }
                    }
                }

                val.content
                    .iter()
                    .map(|Content(content_type, body, example, examples)| {
                        let content = create_content(body, example, examples);
                        (Cow::Borrowed(&**content_type), content)
                    })
                    .for_each(|(content_type, content)| {
                        tokens.extend(quote! { .content(#content_type, #content) })
                    });

                val.headers.iter().for_each(|header| {
                    let name = &header.name;
                    tokens.extend(quote! {
                        .header(#name, #header)
                    })
                });

                tokens.extend(quote! { .build() });
            }
        }
    }
}

trait DeriveResponseValue: Parse {
    fn merge_from(self, other: Self) -> Self;

    fn from_attributes(attributes: &[Attribute]) -> Option<Self> {
        attributes
            .iter()
            .filter(|attribute| attribute.path.get_ident().unwrap() == "response")
            .map(|attribute| attribute.parse_args::<Self>().unwrap_or_abort())
            .reduce(|acc, item| acc.merge_from(item))
    }
}

#[derive(Default)]
#[cfg_attr(feature = "debug", derive(Debug))]
struct DeriveToResponseValue {
    content_type: Option<Vec<String>>,
    headers: Vec<Header>,
    description: String,
    example: Option<(AnyValue, Ident)>,
    examples: Option<(Punctuated<Example, Comma>, Ident)>,
}

impl DeriveResponseValue for DeriveToResponseValue {
    fn merge_from(mut self, other: Self) -> Self {
        if other.content_type.is_some() {
            self.content_type = other.content_type;
        }
        if !other.headers.is_empty() {
            self.headers = other.headers;
        }
        if !other.description.is_empty() {
            self.description = other.description;
        }
        if other.example.is_some() {
            self.example = other.example;
        }
        if other.examples.is_some() {
            self.examples = other.examples;
        }

        self
    }
}

impl Parse for DeriveToResponseValue {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut response = DeriveToResponseValue::default();

        while !input.is_empty() {
            let ident = input.parse::<Ident>()?;
            let attribute_name = &*ident.to_string();

            match attribute_name {
                "description" => {
                    response.description = parse::description(input)?;
                }
                "content_type" => {
                    response.content_type = Some(parse::content_type(input)?);
                }
                "headers" => {
                    response.headers = parse::headers(input)?;
                }
                "example" => {
                    response.example = Some((parse::example(input)?, ident));
                }
                "examples" => {
                    response.examples = Some((parse::examples(input)?, ident));
                }
                _ => {
                    return Err(Error::new(
                        ident.span(),
                        format!("unexected attribute: {attribute_name}, expected any of: inline, description, content_type, headers, example"),
                    ));
                }
            }

            if !input.is_empty() {
                input.parse::<Comma>()?;
            }
        }

        Ok(response)
    }
}

#[derive(Default)]
struct DeriveIntoResponsesValue {
    status: ResponseStatus,
    content_type: Option<Vec<String>>,
    headers: Vec<Header>,
    description: String,
    example: Option<(AnyValue, Ident)>,
    examples: Option<(Punctuated<Example, Comma>, Ident)>,
}

impl DeriveResponseValue for DeriveIntoResponsesValue {
    fn merge_from(mut self, other: Self) -> Self {
        self.status = other.status;

        if other.content_type.is_some() {
            self.content_type = other.content_type;
        }
        if !other.headers.is_empty() {
            self.headers = other.headers;
        }
        if !other.description.is_empty() {
            self.description = other.description;
        }
        if other.example.is_some() {
            self.example = other.example;
        }
        if other.examples.is_some() {
            self.examples = other.examples;
        }

        self
    }
}

impl Parse for DeriveIntoResponsesValue {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut response = DeriveIntoResponsesValue::default();
        const MISSING_STATUS_ERROR: &str = "missing expected `status` attribute";
        let first_span = input.span();

        let status_ident = input
            .parse::<Ident>()
            .map_err(|error| Error::new(error.span(), MISSING_STATUS_ERROR))?;

        if status_ident == "status" {
            response.status = parse_utils::parse_next(input, || input.parse::<ResponseStatus>())?;
        } else {
            return Err(Error::new(status_ident.span(), MISSING_STATUS_ERROR));
        }

        if response.status.to_token_stream().is_empty() {
            return Err(Error::new(first_span, MISSING_STATUS_ERROR));
        }

        while !input.is_empty() {
            let ident = input.parse::<Ident>()?;
            let attribute_name = &*ident.to_string();

            match attribute_name {
                "description" => {
                    response.description = parse::description(input)?;
                }
                "content_type" => {
                    response.content_type = Some(parse::content_type(input)?);
                }
                "headers" => {
                    response.headers = parse::headers(input)?;
                }
                "example" => {
                    response.example = Some((parse::example(input)?, ident));
                }
                "examples" => {
                    response.examples = Some((parse::examples(input)?, ident));
                }
                _ => {
                    return Err(Error::new(
                        ident.span(),
                        format!("unexected attribute: {attribute_name}, expected any of: description, content_type, headers, example, examples"),
                    ));
                }
            }

            if !input.is_empty() {
                input.parse::<Comma>()?;
            }
        }

        Ok(response)
    }
}

#[derive(Default)]
#[cfg_attr(feature = "debug", derive(Debug))]
struct ResponseStatus(TokenStream2);

impl Parse for ResponseStatus {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        fn parse_lit_int(input: ParseStream) -> syn::Result<Cow<'_, str>> {
            input.parse::<LitInt>()?.base10_parse().map(Cow::Owned)
        }

        fn parse_lit_str_status_range(input: ParseStream) -> syn::Result<Cow<'_, str>> {
            const VALID_STATUS_RANGES: [&str; 6] = ["default", "1XX", "2XX", "3XX", "4XX", "5XX"];

            input
                .parse::<LitStr>()
                .and_then(|lit_str| {
                    let value = lit_str.value();
                    if !VALID_STATUS_RANGES.contains(&value.as_str()) {
                        Err(Error::new(
                            value.span(),
                            format!(
                                "Invalid status range, expected one of: {}",
                                VALID_STATUS_RANGES.join(", "),
                            ),
                        ))
                    } else {
                        Ok(value)
                    }
                })
                .map(Cow::Owned)
        }

        fn parse_http_status_code(input: ParseStream) -> syn::Result<TokenStream2> {
            let http_status_path = input.parse::<ExprPath>()?;
            let last_segment = http_status_path
                .path
                .segments
                .last()
                .expect("Expected at least one segment in http StatusCode");

            STATUS_CODES
                .iter()
                .find_map(|(code, name)| {
                    if last_segment.ident == name {
                        Some(code.to_string().to_token_stream())
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    Error::new(
                        last_segment.span(),
                        format!(
                            "No associate item `{}` found for struct `http::StatusCode`",
                            last_segment.ident
                        ),
                    )
                })
        }

        let lookahead = input.lookahead1();
        if lookahead.peek(LitInt) {
            parse_lit_int(input).map(|status| Self(status.to_token_stream()))
        } else if lookahead.peek(LitStr) {
            parse_lit_str_status_range(input).map(|status| Self(status.to_token_stream()))
        } else if lookahead.peek(syn::Ident) {
            parse_http_status_code(input).map(Self)
        } else {
            Err(lookahead.error())
        }
    }
}

impl ToTokens for ResponseStatus {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        self.0.to_tokens(tokens);
    }
}

// content(
//   ("application/json" = Response, example = "...", examples(..., ...)),
//   ("application/json2" = Response2, example = "...", examples("...", "..."))
// )
#[cfg_attr(feature = "debug", derive(Debug))]
struct Content<'c>(
    String,
    PathType<'c>,
    Option<AnyValue>,
    Option<Punctuated<Example, Comma>>,
);

impl Parse for Content<'_> {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content;
        parenthesized!(content in input);

        let content_type = content.parse::<LitStr>()?;
        content.parse::<Token![=]>()?;
        let body = content.parse()?;
        content.parse::<Option<Comma>>()?;
        let mut example = None::<AnyValue>;
        let mut examples = None::<Punctuated<Example, Comma>>;

        while !content.is_empty() {
            let ident = content.parse::<Ident>()?;
            let attribute_name = &*ident.to_string();
            match attribute_name {
                "example" => {
                    example = Some(parse_utils::parse_next(&content, || {
                        AnyValue::parse_json(&content)
                    })?)
                }
                "examples" => {
                    examples = Some(parse_utils::parse_punctuated_within_parenthesis(&content)?)
                }
                _ => {
                    return Err(Error::new(
                        ident.span(),
                        format!(
                            "unexpected attribute: {ident}, expected one of: example, examples"
                        ),
                    ));
                }
            }

            if !content.is_empty() {
                content.parse::<Comma>()?;
            }
        }

        Ok(Content(content_type.value(), body, example, examples))
    }
}

pub struct Responses<'a>(pub &'a [Response<'a>]);

impl ToTokens for Responses<'_> {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        tokens.extend(self.0.iter().fold(
            quote! { utoipa::openapi::ResponsesBuilder::new() },
            |mut acc, response| {
                match response {
                    Response::IntoResponses(path) => {
                        let span = path.span();
                        acc.extend(quote_spanned! {span =>
                            .responses_from_into_responses::<#path>()
                        })
                    }
                    Response::Tuple(response) => {
                        let code = &response.status_code;
                        acc.extend(quote! { .response(#code, #response) });
                    }
                }

                acc
            },
        ));

        tokens.extend(quote! { .build() });
    }
}

/// Parsed representation of response header defined in `#[utoipa::path(..)]` attribute.
///
/// Supported configuration format is `("x-my-header-name" = type, description = "optional description of header")`.
/// The `= type` and the `descripiton = ".."` are optional configurations thus so the same configuration
/// could be written as follows: `("x-my-header-name")`.
///
/// The `type` can be any typical type supported as a header argument such as `String, i32, u64, bool` etc.
/// and if not provided it will default to `String`.
///
/// # Examples
///
/// Example of 200 success response which does return nothing back in response body, but returns a
/// new csrf token in response headers.
/// ```text
/// #[utoipa::path(
///     ...
///     responses = [
///         (status = 200, description = "success response",
///             headers = [
///                 ("xrfs-token" = String, description = "New csrf token sent back in response header")
///             ]
///         ),
///     ]
/// )]
/// ```
///
/// Example with default values.
/// ```text
/// #[utoipa::path(
///     ...
///     responses = [
///         (status = 200, description = "success response",
///             headers = [
///                 ("xrfs-token")
///             ]
///         ),
///     ]
/// )]
/// ```
///
/// Example with multiplea headers with default values.
/// ```text
/// #[utoipa::path(
///     ...
///     responses = [
///         (status = 200, description = "success response",
///             headers = [
///                 ("xrfs-token"),
///                 ("another-header"),
///             ]
///         ),
///     ]
/// )]
/// ```
#[derive(Default)]
#[cfg_attr(feature = "debug", derive(Debug))]
struct Header {
    name: String,
    value_type: Option<InlineType<'static>>,
    description: Option<String>,
}

impl Parse for Header {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut header = Header {
            name: input.parse::<LitStr>()?.value(),
            ..Default::default()
        };

        if input.peek(Token![=]) {
            input.parse::<Token![=]>()?;

            header.value_type = Some(input.parse().map_err(|error| {
                Error::new(
                    error.span(),
                    format!("unexpected token, expected type such as String, {}", error),
                )
            })?);
        }

        if !input.is_empty() {
            input.parse::<Token![,]>()?;
        }

        if input.peek(syn::Ident) {
            input
                .parse::<Ident>()
                .map_err(|error| {
                    Error::new(
                        error.span(),
                        format!("unexpected attribute, expected: description, {}", error),
                    )
                })
                .and_then(|ident| {
                    if ident != "description" {
                        return Err(Error::new(
                            ident.span(),
                            "unexpected attribute, expected: description",
                        ));
                    }
                    Ok(ident)
                })?;
            input.parse::<Token![=]>()?;
            header.description = Some(input.parse::<LitStr>()?.value());
        }

        Ok(header)
    }
}

impl ToTokens for Header {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        if let Some(header_type) = &self.value_type {
            // header property with custom type
            let type_tree = header_type.as_type_tree();
            let media_type_schema = MediaTypeSchema {
                type_tree: &type_tree,
                is_inline: header_type.is_inline,
            };

            tokens.extend(quote! {
                utoipa::openapi::HeaderBuilder::new().schema(#media_type_schema)
            })
        } else {
            // default header (string type)
            tokens.extend(quote! {
                Into::<utoipa::openapi::HeaderBuilder>::into(utoipa::openapi::Header::default())
            })
        };

        if let Some(ref description) = self.description {
            tokens.extend(quote! {
                .description(Some(#description))
            })
        }

        tokens.extend(quote! { .build() })
    }
}

mod parse {
    use syn::parse::ParseStream;
    use syn::punctuated::Punctuated;
    use syn::token::{Bracket, Comma};
    use syn::{bracketed, parenthesized, LitStr, Result};

    use crate::path::example::Example;
    use crate::{parse_utils, AnyValue};

    use super::Header;

    #[inline]
    pub(super) fn description(input: ParseStream) -> Result<String> {
        parse_utils::parse_next_literal_str(input)
    }

    #[inline]
    pub(super) fn content_type(input: ParseStream) -> Result<Vec<String>> {
        parse_utils::parse_next(input, || {
            let look_content_type = input.lookahead1();
            if look_content_type.peek(LitStr) {
                Ok(vec![input.parse::<LitStr>()?.value()])
            } else if look_content_type.peek(Bracket) {
                let content_types;
                bracketed!(content_types in input);
                Ok(
                    Punctuated::<LitStr, Comma>::parse_terminated(&content_types)?
                        .into_iter()
                        .map(|lit| lit.value())
                        .collect(),
                )
            } else {
                Err(look_content_type.error())
            }
        })
    }

    #[inline]
    pub(super) fn headers(input: ParseStream) -> Result<Vec<Header>> {
        let headers;
        parenthesized!(headers in input);

        parse_utils::parse_groups(&headers)
    }

    #[inline]
    pub(super) fn example(input: ParseStream) -> Result<AnyValue> {
        parse_utils::parse_next(input, || AnyValue::parse_lit_str_or_json(input))
    }

    #[inline]
    pub(super) fn examples(input: ParseStream) -> Result<Punctuated<Example, Comma>> {
        parse_utils::parse_punctuated_within_parenthesis(input)
    }
}
