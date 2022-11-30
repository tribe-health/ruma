//! `PUT /_matrix/client/*/pushrules/{scope}/{kind}/{ruleId}`
//!
//! This endpoint allows the creation and modification of push rules for this user ID.

pub mod v3 {
    //! `/v3/` ([spec])
    //!
    //! [spec]: https://spec.matrix.org/v1.4/client-server-api/#put_matrixclientv3pushrulesscopekindruleid

    #[cfg(feature = "server")]
    use ruma_common::api::error::FromHttpRequestError;
    use ruma_common::{
        api::{response, IntoHttpBody, Metadata, RawHttpBody, TryFromHttpBody},
        metadata,
        push::{Action, NewPushRule, PushCondition},
    };
    use serde::{Deserialize, Serialize};

    use crate::push::RuleScope;

    const METADATA: Metadata = metadata! {
        method: PUT,
        rate_limited: true,
        authentication: AccessToken,
        history: {
            1.0 => "/_matrix/client/r0/pushrules/:scope/:kind/:rule_id",
            1.1 => "/_matrix/client/v3/pushrules/:scope/:kind/:rule_id",
        }
    };

    /// Request type for the `set_pushrule` endpoint.
    #[derive(Clone, Debug)]
    #[cfg_attr(not(feature = "unstable-exhaustive-types"), non_exhaustive)]
    pub struct Request {
        /// The scope to set the rule in.
        pub scope: RuleScope,

        /// The rule.
        pub rule: NewPushRule,

        /// Use 'before' with a rule_id as its value to make the new rule the next-most important
        /// rule with respect to the given user defined rule.
        pub before: Option<String>,

        /// This makes the new rule the next-less important rule relative to the given user defined
        /// rule.
        pub after: Option<String>,
    }

    /// Response type for the `set_pushrule` endpoint.
    #[response(error = crate::Error)]
    #[derive(Default)]
    pub struct Response {}

    impl Request {
        /// Creates a new `Request` with the given scope and rule.
        pub fn new(scope: RuleScope, rule: NewPushRule) -> Self {
            Self { scope, rule, before: None, after: None }
        }
    }

    impl Response {
        /// Creates an empty `Response`.
        pub fn new() -> Self {
            Self {}
        }
    }

    #[cfg(feature = "client")]
    impl ruma_common::api::OutgoingRequest for Request {
        type OutgoingBody = impl IntoHttpBody;
        type EndpointError = crate::Error;
        type IncomingResponse = Response;

        const METADATA: Metadata = METADATA;

        fn try_into_http_request(
            self,
            base_url: &str,
            access_token: ruma_common::api::SendAccessToken<'_>,
            considering_versions: &[ruma_common::api::MatrixVersion],
        ) -> Result<http::Request<Self::OutgoingBody>, ruma_common::api::error::IntoHttpError>
        {
            use http::header;
            use ruma_common::serde::urlencoded;

            let query_string =
                urlencoded::to_string(RequestQuery { before: self.before, after: self.after })?;

            let url = METADATA.make_endpoint_url(
                considering_versions,
                base_url,
                &[&self.scope, &self.rule.kind(), &self.rule.rule_id()],
                &query_string,
            )?;

            let body: RequestBody = self.rule.into();

            http::Request::builder()
                .method(http::Method::GET)
                .uri(url)
                .header(header::CONTENT_TYPE, "application/json")
                .header(
                    header::AUTHORIZATION,
                    format!(
                        "Bearer {}",
                        access_token
                            .get_required_for_endpoint()
                            .ok_or(ruma_common::api::error::IntoHttpError::NeedsAuthentication)?,
                    ),
                )
                .body(body)
                .map_err(Into::into)
        }
    }

    #[cfg(feature = "server")]
    impl ruma_common::api::IncomingRequest for Request {
        type IncomingBody = impl TryFromHttpBody<FromHttpRequestError>;
        type EndpointError = crate::Error;
        type OutgoingResponse = Response;

        const METADATA: Metadata = METADATA;

        fn try_from_http_request<S>(
            request: http::Request<Self::IncomingBody>,
            path_args: &[S],
        ) -> Result<Self, FromHttpRequestError>
        where
            S: AsRef<str>,
        {
            use ruma_common::push::{
                NewConditionalPushRule, NewPatternedPushRule, NewSimplePushRule,
            };

            // Exhaustive enum to fail deserialization on unknown variants.
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "lowercase")]
            enum RuleKind {
                Override,
                Underride,
                Sender,
                Room,
                Content,
            }

            #[derive(Deserialize)]
            struct IncomingRequestQuery {
                before: Option<String>,
                after: Option<String>,
            }

            let (scope, kind, rule_id): (RuleScope, RuleKind, String) =
                serde::Deserialize::deserialize(serde::de::value::SeqDeserializer::<
                    _,
                    serde::de::value::Error,
                >::new(
                    path_args.iter().map(::std::convert::AsRef::as_ref),
                ))?;

            let IncomingRequestQuery { before, after } =
                ruma_common::serde::urlencoded::from_str(request.uri().query().unwrap_or(""))?;

            let body: RawHttpBody = request.into_body();

            let rule = match kind {
                RuleKind::Override => {
                    let ConditionalRequestBody { actions, conditions } =
                        serde_json::from_slice(&body.0)?;
                    NewPushRule::Override(NewConditionalPushRule::new(rule_id, conditions, actions))
                }
                RuleKind::Underride => {
                    let ConditionalRequestBody { actions, conditions } =
                        serde_json::from_slice(&body.0)?;
                    NewPushRule::Underride(NewConditionalPushRule::new(
                        rule_id, conditions, actions,
                    ))
                }
                RuleKind::Sender => {
                    let SimpleRequestBody { actions } = serde_json::from_slice(&body.0)?;
                    let rule_id = rule_id.try_into()?;
                    NewPushRule::Sender(NewSimplePushRule::new(rule_id, actions))
                }
                RuleKind::Room => {
                    let SimpleRequestBody { actions } = serde_json::from_slice(&body.0)?;
                    let rule_id = rule_id.try_into()?;
                    NewPushRule::Room(NewSimplePushRule::new(rule_id, actions))
                }
                RuleKind::Content => {
                    let PatternedRequestBody { actions, pattern } =
                        serde_json::from_slice(&body.0)?;
                    NewPushRule::Content(NewPatternedPushRule::new(rule_id, pattern, actions))
                }
            };

            Ok(Self { scope, rule, before, after })
        }
    }

    #[derive(Debug, Serialize)]
    struct RequestQuery {
        #[serde(skip_serializing_if = "Option::is_none")]
        before: Option<String>,

        #[serde(skip_serializing_if = "Option::is_none")]
        after: Option<String>,
    }

    #[derive(Debug, Serialize)]
    #[serde(untagged)]
    enum RequestBody {
        Simple(SimpleRequestBody),

        Patterned(PatternedRequestBody),

        Conditional(ConditionalRequestBody),
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct SimpleRequestBody {
        actions: Vec<Action>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct PatternedRequestBody {
        actions: Vec<Action>,

        pattern: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct ConditionalRequestBody {
        actions: Vec<Action>,

        conditions: Vec<PushCondition>,
    }

    impl From<NewPushRule> for RequestBody {
        fn from(rule: NewPushRule) -> Self {
            match rule {
                NewPushRule::Override(r) => RequestBody::Conditional(ConditionalRequestBody {
                    actions: r.actions,
                    conditions: r.conditions,
                }),
                NewPushRule::Content(r) => RequestBody::Patterned(PatternedRequestBody {
                    actions: r.actions,
                    pattern: r.pattern,
                }),
                NewPushRule::Room(r) => {
                    RequestBody::Simple(SimpleRequestBody { actions: r.actions })
                }
                NewPushRule::Sender(r) => {
                    RequestBody::Simple(SimpleRequestBody { actions: r.actions })
                }
                NewPushRule::Underride(r) => RequestBody::Conditional(ConditionalRequestBody {
                    actions: r.actions,
                    conditions: r.conditions,
                }),
                #[cfg(not(feature = "unstable-exhaustive-types"))]
                _ => unreachable!("variant added to NewPushRule not covered by RequestBody"),
            }
        }
    }
}
