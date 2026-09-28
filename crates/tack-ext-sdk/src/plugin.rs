//! The plugin builder, request dispatcher, and serve loop.

use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::host::{Cx, State};
use crate::{Error, PROTOCOL_VERSION};
use tack_ext::rpc3::{
    AfterToolCallParams, AfterToolCallPatch, ApprovalDecision, ApprovalReviewParams,
    AutocompleteProvideParams, AutocompleteProvideResult, AutocompleteProviderSpec,
    BeforeToolCallParams, CommandInvokeParams, CommandSpec, ConfigDeclaration, ErrorObject,
    HookCapabilities, InitializeParams, InitializeResult, LifecycleEventParams, MetricsDeclaration,
    PluginCapabilities, PluginInfo, ToolExecuteParams, ToolOutput, ToolSpec,
    TransformContextParams, TransformContextResult, Verdict, WidgetActionParams, WidgetSpec,
    method,
};
use tack_ext::v3::peer::PeerHandler;
use tack_ext::v3::{JsonRpcPeer, protocol_compatible};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

type ToolHandler =
    Arc<dyn Fn(ToolExecuteParams, Cx) -> BoxFuture<Result<ToolOutput, Error>> + Send + Sync>;
type CommandHandler =
    Arc<dyn Fn(CommandInvokeParams, Cx) -> BoxFuture<Result<Value, Error>> + Send + Sync>;
type BeforeHandler =
    Arc<dyn Fn(BeforeToolCallParams, Cx) -> BoxFuture<Result<Verdict, Error>> + Send + Sync>;
type AfterHandler = Arc<
    dyn Fn(AfterToolCallParams, Cx) -> BoxFuture<Result<Option<AfterToolCallPatch>, Error>>
        + Send
        + Sync,
>;
type TransformHandler = Arc<
    dyn Fn(TransformContextParams, Cx) -> BoxFuture<Result<Option<TransformContextResult>, Error>>
        + Send
        + Sync,
>;
type ApprovalHandler = Arc<
    dyn Fn(ApprovalReviewParams, Cx) -> BoxFuture<Result<Option<ApprovalDecision>, Error>>
        + Send
        + Sync,
>;
type EventHandler = Arc<dyn Fn(LifecycleEventParams, Cx) -> BoxFuture<()> + Send + Sync>;
type WidgetActionHandler = Arc<dyn Fn(WidgetActionParams, Cx) -> BoxFuture<()> + Send + Sync>;
type AutocompleteHandler = Arc<
    dyn Fn(AutocompleteProvideParams, Cx) -> BoxFuture<Result<AutocompleteProvideResult, Error>>
        + Send
        + Sync,
>;

/// Builds a [`Plugin`]. Every capability is optional and independent;
/// undeclared capabilities cost nothing (the host skips the calls).
pub struct PluginBuilder {
    name: String,
    version: Option<String>,
    description: Option<String>,
    tools: Vec<(ToolSpec, ToolHandler)>,
    commands: Vec<(CommandSpec, CommandHandler)>,
    before_tool_call: Option<BeforeHandler>,
    after_tool_call: Option<AfterHandler>,
    transform_context: Option<TransformHandler>,
    approval_review: Option<ApprovalHandler>,
    events: Vec<String>,
    event_handler: Option<EventHandler>,
    widgets: Vec<WidgetSpec>,
    widget_action_handler: Option<WidgetActionHandler>,
    autocomplete: Vec<(AutocompleteProviderSpec, AutocompleteHandler)>,
    config_schema: Option<Value>,
    metrics: Option<MetricsDeclaration>,
}

impl std::fmt::Debug for PluginBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginBuilder")
            .field("name", &self.name)
            .finish()
    }
}

impl PluginBuilder {
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Contribute a tool. `spec.parameters` must be a JSON object schema.
    pub fn tool<F, Fut>(mut self, spec: ToolSpec, handler: F) -> Self
    where
        F: Fn(ToolExecuteParams, Cx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, Error>> + Send + 'static,
    {
        self.tools.push((
            spec,
            Arc::new(move |p, cx| Box::pin(handler(p, cx)) as BoxFuture<_>),
        ));
        self
    }

    /// Contribute a slash command.
    pub fn command<F, Fut>(
        mut self,
        name: impl Into<String>,
        description: Option<String>,
        handler: F,
    ) -> Self
    where
        F: Fn(CommandInvokeParams, Cx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, Error>> + Send + 'static,
    {
        self.commands.push((
            CommandSpec {
                name: name.into(),
                description,
            },
            Arc::new(move |p, cx| Box::pin(handler(p, cx)) as BoxFuture<_>),
        ));
        self
    }

    /// Intercept tool calls: allow / deny / rewrite. Chained by the host;
    /// later plugins observe earlier rewrites, the first deny wins.
    pub fn before_tool_call<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(BeforeToolCallParams, Cx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Verdict, Error>> + Send + 'static,
    {
        self.before_tool_call = Some(Arc::new(move |p, cx| {
            Box::pin(handler(p, cx)) as BoxFuture<_>
        }));
        self
    }

    /// Observe and patch tool results (`None` = no patch).
    pub fn after_tool_call<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(AfterToolCallParams, Cx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<AfterToolCallPatch>, Error>> + Send + 'static,
    {
        self.after_tool_call = Some(Arc::new(move |p, cx| {
            Box::pin(handler(p, cx)) as BoxFuture<_>
        }));
        self
    }

    /// Rewrite the model context (`None` = unchanged; COW pipeline).
    pub fn transform_context<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(TransformContextParams, Cx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<TransformContextResult>, Error>> + Send + 'static,
    {
        self.transform_context = Some(Arc::new(move |p, cx| {
            Box::pin(handler(p, cx)) as BoxFuture<_>
        }));
        self
    }

    /// Participate in the approval chain (`None` = pass to the next
    /// reviewer; first claim wins).
    pub fn approval_review<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(ApprovalReviewParams, Cx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<ApprovalDecision>, Error>> + Send + 'static,
    {
        self.approval_review = Some(Arc::new(move |p, cx| {
            Box::pin(handler(p, cx)) as BoxFuture<_>
        }));
        self
    }

    /// Subscribe to lifecycle events (names like `turnStart`; empty list
    /// = the host's default set).
    pub fn events<F, Fut>(mut self, events: &[&str], handler: F) -> Self
    where
        F: Fn(LifecycleEventParams, Cx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.events = events.iter().map(|s| (*s).to_string()).collect();
        self.event_handler = Some(Arc::new(move |p, cx| {
            Box::pin(handler(p, cx)) as BoxFuture<_>
        }));
        self
    }

    /// Declare a widget (the host owns rendering; push state via
    /// [`crate::Host::widget_update`]).
    pub fn widget(mut self, spec: WidgetSpec) -> Self {
        self.widgets.push(spec);
        self
    }

    /// Handle widget interactions (for example list selections).
    pub fn on_widget_action<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(WidgetActionParams, Cx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.widget_action_handler = Some(Arc::new(move |p, cx| {
            Box::pin(handler(p, cx)) as BoxFuture<_>
        }));
        self
    }

    /// Contribute an autocomplete provider.
    pub fn autocomplete<F, Fut>(mut self, spec: AutocompleteProviderSpec, handler: F) -> Self
    where
        F: Fn(AutocompleteProvideParams, Cx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<AutocompleteProvideResult, Error>> + Send + 'static,
    {
        self.autocomplete.push((
            spec,
            Arc::new(move |p, cx| Box::pin(handler(p, cx)) as BoxFuture<_>),
        ));
        self
    }

    /// Declare the per-plugin config JSON Schema (the host validates
    /// settings against it; the plugin reads the result from
    /// [`Cx::config`]).
    pub fn config_schema(mut self, schema: Value) -> Self {
        self.config_schema = Some(schema);
        self
    }

    /// Declare the metrics schema for the sidecar (operations +
    /// dimension enums).
    pub fn metrics(mut self, declaration: MetricsDeclaration) -> Self {
        self.metrics = Some(declaration);
        self
    }

    pub fn build(self) -> Plugin {
        Plugin { builder: self }
    }

    /// Build and serve over stdio (shortcut for `build().run()`).
    pub async fn run(self) -> std::io::Result<()> {
        self.build().run().await
    }
}

/// A built plugin, ready to serve.
pub struct Plugin {
    builder: PluginBuilder,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("name", &self.builder.name)
            .finish()
    }
}

impl Plugin {
    pub fn builder(name: impl Into<String>) -> PluginBuilder {
        PluginBuilder {
            name: name.into(),
            version: None,
            description: None,
            tools: Vec::new(),
            commands: Vec::new(),
            before_tool_call: None,
            after_tool_call: None,
            transform_context: None,
            approval_review: None,
            events: Vec::new(),
            event_handler: None,
            widgets: Vec::new(),
            widget_action_handler: None,
            autocomplete: Vec::new(),
            config_schema: None,
            metrics: None,
        }
    }

    /// Serve over stdio (the process carrier). Returns on `shutdown` or
    /// when the host closes the transport.
    ///
    /// stdout is the RPC bus — never print to it; use
    /// [`crate::Host::log`] instead.
    pub async fn run(self) -> std::io::Result<()> {
        self.run_on(tokio::io::stdin(), tokio::io::stdout()).await
    }

    /// Serve over an arbitrary transport (tests, the WASM debug carrier).
    pub async fn run_on<R, W>(self, reader: R, writer: W) -> std::io::Result<()>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let state = Arc::new(State::default());
        let dispatch = Arc::new(Dispatch {
            plugin: self,
            state: state.clone(),
        });
        let peer = JsonRpcPeer::new(reader, writer, dispatch);
        let _ = state.peer.set(peer.clone());
        tokio::select! {
            () = state.shutdown.notified() => {}
            () = peer.wait_dead() => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

struct Dispatch {
    plugin: Plugin,
    state: Arc<State>,
}

impl Dispatch {
    fn cx(&self) -> Cx {
        Cx {
            state: self.state.clone(),
        }
    }

    fn on_initialize(&self, params: Value) -> Result<Value, ErrorObject> {
        let params: InitializeParams = parse(params)?;
        if !protocol_compatible(&params.protocol_version) {
            return Err(Error::invalid_params(format!(
                "unsupported host protocol {} (this plugin speaks {PROTOCOL_VERSION})",
                params.protocol_version
            ))
            .into());
        }
        let builder = &self.plugin.builder;
        let hooks = HookCapabilities {
            before_tool_call: builder.before_tool_call.as_ref().map(|_| true),
            transform_context: builder.transform_context.as_ref().map(|_| true),
            after_tool_call: builder.after_tool_call.as_ref().map(|_| true),
            approval_review: builder.approval_review.as_ref().map(|_| true),
        };
        let any_hooks = builder.before_tool_call.is_some()
            || builder.after_tool_call.is_some()
            || builder.transform_context.is_some()
            || builder.approval_review.is_some();
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION.to_string(),
            plugin: PluginInfo {
                name: builder.name.clone(),
                version: builder.version.clone(),
                description: builder.description.clone(),
            },
            capabilities: PluginCapabilities {
                tools: (!builder.tools.is_empty())
                    .then(|| builder.tools.iter().map(|(spec, _)| spec.clone()).collect()),
                commands: (!builder.commands.is_empty()).then(|| {
                    builder
                        .commands
                        .iter()
                        .map(|(spec, _)| spec.clone())
                        .collect()
                }),
                hooks: any_hooks.then_some(hooks),
                events: builder
                    .event_handler
                    .as_ref()
                    .map(|_| builder.events.clone()),
                widgets: (!builder.widgets.is_empty()).then(|| builder.widgets.clone()),
                autocomplete_providers: (!builder.autocomplete.is_empty()).then(|| {
                    builder
                        .autocomplete
                        .iter()
                        .map(|(spec, _)| spec.clone())
                        .collect()
                }),
                config: builder
                    .config_schema
                    .clone()
                    .map(|schema| ConfigDeclaration { schema }),
                metrics: builder.metrics.clone(),
            },
        };
        let _ = self.state.init.set(params);
        to_value(result)
    }
}

#[async_trait::async_trait]
impl PeerHandler for Dispatch {
    async fn handle_request(&self, rpc_method: &str, params: Value) -> Result<Value, ErrorObject> {
        let builder = &self.plugin.builder;
        match rpc_method {
            method::INITIALIZE => self.on_initialize(params),
            method::SHUTDOWN => {
                self.state.shutdown.notify_one();
                Ok(Value::Null)
            }
            method::TOOLS_EXECUTE => {
                let params: ToolExecuteParams = parse(params)?;
                let Some((_, handler)) = builder
                    .tools
                    .iter()
                    .find(|(spec, _)| spec.name == params.name)
                else {
                    return Err(
                        Error::invalid_params(format!("unknown tool {:?}", params.name)).into(),
                    );
                };
                to_value(handler(params, self.cx()).await?)
            }
            method::COMMANDS_INVOKE => {
                let params: CommandInvokeParams = parse(params)?;
                let Some((_, handler)) = builder
                    .commands
                    .iter()
                    .find(|(spec, _)| spec.name == params.name)
                else {
                    return Err(Error::invalid_params(format!(
                        "unknown command {:?}",
                        params.name
                    ))
                    .into());
                };
                let result = handler(params, self.cx()).await?;
                Ok(result)
            }
            method::HOOKS_BEFORE_TOOL_CALL => {
                let Some(handler) = &builder.before_tool_call else {
                    return Err(not_granted(rpc_method));
                };
                to_value(handler(parse(params)?, self.cx()).await?)
            }
            method::HOOKS_AFTER_TOOL_CALL => {
                let Some(handler) = &builder.after_tool_call else {
                    return Err(not_granted(rpc_method));
                };
                to_value(handler(parse(params)?, self.cx()).await?)
            }
            method::HOOKS_TRANSFORM_CONTEXT => {
                let Some(handler) = &builder.transform_context else {
                    return Err(not_granted(rpc_method));
                };
                to_value(handler(parse(params)?, self.cx()).await?)
            }
            method::APPROVAL_REVIEW => {
                let Some(handler) = &builder.approval_review else {
                    return Err(not_granted(rpc_method));
                };
                to_value(handler(parse(params)?, self.cx()).await?)
            }
            method::AUTOCOMPLETE_PROVIDE => {
                let params: AutocompleteProvideParams = parse(params)?;
                let Some((_, handler)) = builder
                    .autocomplete
                    .iter()
                    .find(|(spec, _)| spec.id == params.provider_id)
                else {
                    return Err(Error::invalid_params(format!(
                        "unknown autocomplete provider {:?}",
                        params.provider_id
                    ))
                    .into());
                };
                to_value(handler(params, self.cx()).await?)
            }
            other => Err(Error::new(
                tack_ext::rpc3::ERR_METHOD_NOT_FOUND,
                format!("unknown method {other}"),
            )
            .into()),
        }
    }

    async fn handle_notification(&self, rpc_method: &str, params: Value) {
        match rpc_method {
            method::EVENTS_LIFECYCLE => {
                if let Some(handler) = &self.plugin.builder.event_handler
                    && let Ok(params) = serde_json::from_value::<LifecycleEventParams>(params)
                {
                    handler(params, self.cx()).await;
                }
            }
            method::WIDGETS_ACTION => {
                if let Some(handler) = &self.plugin.builder.widget_action_handler
                    && let Ok(params) = serde_json::from_value::<WidgetActionParams>(params)
                {
                    handler(params, self.cx()).await;
                }
            }
            _ => {}
        }
    }
}

fn parse<P: DeserializeOwned>(params: Value) -> Result<P, ErrorObject> {
    serde_json::from_value(params).map_err(|e| Error::invalid_params(e.to_string()).into())
}

fn to_value<T: Serialize>(value: T) -> Result<Value, ErrorObject> {
    serde_json::to_value(value).map_err(|e| Error::internal(e.to_string()).into())
}

fn not_granted(rpc_method: &str) -> ErrorObject {
    Error::new(
        tack_ext::rpc3::ERR_CAPABILITY_NOT_GRANTED,
        format!("capability not declared for {rpc_method}"),
    )
    .into()
}
