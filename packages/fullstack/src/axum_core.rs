//! Dioxus core utilities for the [Axum](https://docs.rs/axum/latest/axum/index.html) server framework.
//!
//! # Example
//! ```rust, no_run
//! #![allow(non_snake_case)]
//! use dioxus::prelude::*;
//!
//! fn main() {
//!     #[cfg(feature = "web")]
//!     // Hydrate the application on the client
//!     dioxus::launch(app);
//!     #[cfg(feature = "server")]
//!     {
//!         tokio::runtime::Runtime::new()
//!             .unwrap()
//!             .block_on(async move {
//!                 // Get the address the server should run on. If the CLI is running, the CLI proxies fullstack into the main address
//!                 // and we use the generated address the CLI gives us
//!                 let address = dioxus::cli_config::fullstack_address_or_localhost();
//!                 let listener = tokio::net::TcpListener::bind(address)
//!                     .await
//!                     .unwrap();
//!                 axum::serve(
//!                         listener,
//!                         axum::Router::new()
//!                             // Server side render the application, serve static assets, and register server functions
//!                             .register_server_functions()
//!                             .fallback(get(render_handler)
//!                                 // Note: ServeConfig::new won't work on WASM
//!                                 .with_state(RenderHandler::new(ServeConfig::new().unwrap(), app))
//!                             )
//!                             .into_make_service(),
//!                     )
//!                     .await
//!                     .unwrap();
//!             });
//!      }
//! }
//!
//! fn app() -> Element {
//!     let mut text = use_signal(|| "...".to_string());
//!
//!     rsx! {
//!         button {
//!             onclick: move |_| async move {
//!                 if let Ok(data) = get_server_data().await {
//!                     text.set(data);
//!                 }
//!             },
//!             "Run a server function"
//!         }
//!         "Server said: {text}"
//!     }
//! }
//!
//! #[server(GetServerData)]
//! async fn get_server_data() -> Result<String, ServerFnError> {
//!     Ok("Hello from the server!".to_string())
//! }
//!
//! # WASM support
//!
//! These utilities compile to the WASM family of targets, while the more complete ones found in [server] don't
//! ```

use std::sync::Arc;

use crate::prelude::*;
use crate::render::SSRError;
use crate::ContextProviders;

use axum::body;
use axum::extract::State;
use axum::routing::*;
use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    response::IntoResponse,
};
use dioxus_lib::prelude::{Element, VirtualDom};
use http::header::*;

/// A extension trait with server function utilities for integrating Dioxus with your Axum router.
pub trait DioxusRouterFnExt<S> {
    /// Registers server functions with the default handler. This handler function will pass an empty [`DioxusServerContext`] to your server functions.
    ///
    /// # Example
    /// ```rust, no_run
    /// # use dioxus_lib::prelude::*;
    /// # use dioxus_fullstack::prelude::*;
    /// #[tokio::main]
    /// async fn main() {
    ///     let addr = dioxus::cli_config::fullstack_address_or_localhost();
    ///     let router = axum::Router::new()
    ///         // Register server functions routes with the default handler
    ///         .register_server_functions()
    ///         .into_make_service();
    ///     let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    ///     axum::serve(listener, router).await.unwrap();
    /// }
    /// ```
    #[allow(dead_code)]
    fn register_server_functions(self) -> Self
    where
        Self: Sized,
    {
        self.register_server_functions_with_context(Default::default())
    }

    /// Registers server functions with some additional context to insert into the [`DioxusServerContext`] for that handler.
    ///
    /// # Example
    /// ```rust, no_run
    /// # use dioxus_lib::prelude::*;
    /// # use dioxus_fullstack::prelude::*;
    /// # use std::sync::Arc;
    /// #[tokio::main]
    /// async fn main() {
    ///     let addr = dioxus::cli_config::fullstack_address_or_localhost();
    ///     let router = axum::Router::new()
    ///         // Register server functions routes with the default handler
    ///         .register_server_functions_with_context(Arc::new(vec![Box::new(|| Box::new(1234567890u32))]))
    ///         .into_make_service();
    ///     let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    ///     axum::serve(listener, router).await.unwrap();
    /// }
    /// ```
    fn register_server_functions_with_context(self, context_providers: ContextProviders) -> Self;
}

impl<S> DioxusRouterFnExt<S> for Router<S>
where
    S: Send + Sync + Clone + 'static,
{
    fn register_server_functions_with_context(
        mut self,
        context_providers: ContextProviders,
    ) -> Self {
        use http::method::Method;

        for (path, method) in server_fn::axum::server_fn_paths() {
            tracing::trace!("Registering server function: {} {}", method, path);
            let context_providers = context_providers.clone();
            let handler = move |req| handle_server_fns_inner(path, context_providers, req);
            self = match method {
                Method::GET => self.route(path, get(handler)),
                Method::POST => self.route(path, post(handler)),
                Method::PUT => self.route(path, put(handler)),
                _ => unimplemented!("Unsupported server function method: {}", method),
            };
        }

        self
    }
}

/// A handler for Dioxus server functions. This will run the server function and return the result.
async fn handle_server_fns_inner(
    path: &str,
    additional_context: ContextProviders,
    req: Request<Body>,
) -> impl IntoResponse {
    let path_string = path.to_string();

    let (parts, body) = req.into_parts();
    let req = Request::from_parts(parts.clone(), body);
    let method = req.method().clone();

    if let Some(mut service) =
            server_fn::axum::get_server_fn_service(&path_string, method)
        {
            // Create the server context with info from the request
            let server_context = DioxusServerContext::new(parts);
            // Provide additional context from the render state
            add_server_context(&server_context, &additional_context);

            // store Accepts and Referrer in case we need them for redirect (below)
            let accepts_html = req
                .headers()
                .get(ACCEPT)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.contains("text/html"))
                .unwrap_or(false);
            let referrer = req.headers().get(REFERER).cloned();

            // actually run the server fn (which may use the server context)
            let fut = with_server_context(server_context.clone(), || service.run(req));
            let mut res = ProvideServerContext::new(fut, server_context.clone()).await;

            // it it accepts text/html (i.e., is a plain form post) and doesn't already have a
            // Location set, then redirect to Referer
            if accepts_html {
                if let Some(referrer) = referrer {
                    let has_location = res.headers().get(LOCATION).is_some();
                    if !has_location {
                        *res.status_mut() = StatusCode::FOUND;
                        res.headers_mut().insert(LOCATION, referrer);
                    }
                }
            }

            // apply the response parts from the server context to the response
            server_context.send_response(&mut res);

            Ok(res)
        } else {
            Response::builder().status(StatusCode::BAD_REQUEST).body(
                {
                    #[cfg(target_family = "wasm")]
                    {
                        Body::from(format!(
                            "No server function found for path: {path_string}\nYou may need to explicitly register the server function with `register_explicit`, rebuild your wasm binary to update a server function link or make sure the prefix your server and client use for server functions match.",
                        ))
                    }
                    #[cfg(not(target_family = "wasm"))]
                    {
                        Body::from(format!(
                            "No server function found for path: {path_string}\nYou may need to rebuild your wasm binary to update a server function link or make sure the prefix your server and client use for server functions match.",
                        ))
                    }
                }
            )
        }
        .expect("could not build Response")
}

pub(crate) fn add_server_context(
    server_context: &DioxusServerContext,
    context_providers: &ContextProviders,
) {
    for index in 0..context_providers.len() {
        let context_providers = context_providers.clone();
        server_context.insert_boxed_factory(Box::new(move || context_providers[index]()));
    }
}

/// State used by [`render_handler`] to render a dioxus component with axum
#[derive(Clone)]
pub struct RenderHandleState {
    config: ServeConfig,
    build_virtual_dom: Arc<dyn Fn() -> VirtualDom + Send + Sync>,
    ssr_state: once_cell::sync::OnceCell<SSRState>,
}

impl RenderHandleState {
    /// Create a new [`RenderHandleState`]
    pub fn new(config: ServeConfig, root: fn() -> Element) -> Self {
        Self {
            config,
            build_virtual_dom: Arc::new(move || VirtualDom::new(root)),
            ssr_state: Default::default(),
        }
    }

    /// Create a new [`RenderHandleState`] with a custom [`VirtualDom`] factory. This method can be used to pass context into the root component of your application.
    pub fn new_with_virtual_dom_factory(
        config: ServeConfig,
        build_virtual_dom: impl Fn() -> VirtualDom + Send + Sync + 'static,
    ) -> Self {
        Self {
            config,
            build_virtual_dom: Arc::new(build_virtual_dom),
            ssr_state: Default::default(),
        }
    }

    /// Set the [`ServeConfig`] for this [`RenderHandleState`]
    pub fn with_config(mut self, config: ServeConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the [`SSRState`] for this [`RenderHandleState`]. Sharing a [`SSRState`] between multiple [`RenderHandleState`]s is more efficient than creating a new [`SSRState`] for each [`RenderHandleState`].
    pub fn with_ssr_state(mut self, ssr_state: SSRState) -> Self {
        self.ssr_state = once_cell::sync::OnceCell::new();
        if self.ssr_state.set(ssr_state).is_err() {
            panic!("SSRState already set");
        }
        self
    }

    fn ssr_state(&self) -> &SSRState {
        self.ssr_state.get_or_init(|| SSRState::new(&self.config))
    }
}

/// SSR renderer handler for Axum with added context injection.
///
/// # Example
/// ```rust,no_run
/// #![allow(non_snake_case)]
/// use std::sync::{Arc, Mutex};
///
/// use axum::routing::get;
/// use dioxus::prelude::*;
///
/// fn app() -> Element {
///     rsx! {
///         "hello!"
///     }
/// }
///
/// #[tokio::main]
/// async fn main() {
///     let addr = dioxus::cli_config::fullstack_address_or_localhost();
///     let router = axum::Router::new()
///         // Register server functions, etc.
///         // Note you can use `register_server_functions_with_context`
///         // to inject the context into server functions running outside
///         // of an SSR render context.
///         .fallback(get(render_handler)
///             .with_state(RenderHandleState::new(ServeConfig::new().unwrap(), app))
///         )
///         .into_make_service();
///     let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
///     axum::serve(listener, router).await.unwrap();
/// }
/// ```
pub async fn render_handler(
    State(state): State<RenderHandleState>,
    request: Request<Body>,
) -> impl IntoResponse {
    let cfg = &state.config;
    let ssr_state = state.ssr_state();
    let build_virtual_dom = {
        let build_virtual_dom = state.build_virtual_dom.clone();
        let context_providers = state.config.context_providers.clone();
        move || {
            let mut vdom = build_virtual_dom();
            for state in context_providers.as_slice() {
                vdom.insert_any_root_context(state());
            }
            vdom
        }
    };

    let (parts, _) = request.into_parts();
    let url = parts
        .uri
        .path_and_query()
        .ok_or(StatusCode::BAD_REQUEST)?
        .to_string();
    let parts: Arc<parking_lot::RwLock<http::request::Parts>> =
        Arc::new(parking_lot::RwLock::new(parts));
    // Create the server context with info from the request
    let server_context = DioxusServerContext::from_shared_parts(parts.clone());
    // Provide additional context from the render state
    add_server_context(&server_context, &state.config.context_providers);

    match ssr_state
        .render(url, cfg, build_virtual_dom, &server_context)
        .await
    {
        Ok((freshness, rx)) => {
            let mut response = axum::response::Html::from(Body::from_stream(rx)).into_response();
            freshness.write(response.headers_mut());
            server_context.send_response(&mut response);
            Result::<http::Response<axum::body::Body>, StatusCode>::Ok(response)
        }
        Err(SSRError::Incremental(e)) => {
            tracing::error!("Failed to render page: {}", e);
            Ok(report_err(e).into_response())
        }
        Err(SSRError::Routing(e)) => {
            tracing::trace!("Page not found: {}", e);
            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Page not found"))
                .unwrap())
        }
    }
}

fn report_err<E: std::fmt::Display>(e: E) -> Response<axum::body::Body> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(body::Body::new(format!("Error: {}", e)))
        .unwrap()
}
