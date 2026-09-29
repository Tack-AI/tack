// Hand-written API typings for @tack/plugin; protocol shapes come from
// the generated types.d.ts.

import type {
  AfterToolCallParams,
  AfterToolCallPatch,
  ApprovalDecision,
  ApprovalReviewParams,
  AutocompleteProvideParams,
  AutocompleteProvideResult,
  AutocompleteProviderSpec,
  BeforeToolCallParams,
  CommandInvokeParams,
  HostCapabilities,
  InitializeParams,
  LifecycleEventParams,
  LogLevel,
  MetricsDeclaration,
  ProviderEventKind,
  ProviderStreamParams,
  SessionInfo,
  Snapshot,
  ToolExecuteParams,
  ToolOutput,
  ToolSpec,
  TransformContextParams,
  TransformContextResult,
  Verdict,
  WidgetActionParams,
  WidgetSpec,
  WidgetUpdateParams,
  ExecRunResult,
} from "./types";

export declare class PeerError extends Error {
  code: number;
  data?: any;
  constructor(code: number, message: string, data?: any);
}

export declare const PROTOCOL_VERSION: string;

export interface Peer {
  readonly alive: boolean;
  call(method: string, params?: any, timeoutMs?: number): Promise<any>;
  notify(method: string, params?: any): Promise<void>;
}

export declare function createPeer(options: {
  input: AsyncIterable<any>;
  output: { write(s: string, cb?: (err?: Error) => void): unknown };
  handler?: {
    handleRequest?(method: string, params: any): Promise<any>;
    handleNotification?(method: string, params: any): Promise<void>;
  };
}): Peer;

export interface Cx {
  mode?: "tui" | "print" | "rpc" | "acp";
  trusted?: boolean;
  cwd?: string;
  capabilities: Partial<HostCapabilities>;
  config?: any;
  host: Host;
}

export interface Host {
  notify(message: string, level?: LogLevel): Promise<void>;
  select(title: string, options: string[]): Promise<string | null>;
  confirm(title: string, message: string): Promise<boolean>;
  input(title: string, placeholder?: string): Promise<string | null>;
  exec(command: string, timeoutMs?: number): Promise<ExecRunResult>;
  log(level: LogLevel, message: string): Promise<void>;
  warn(message: string, context?: any): Promise<void>;
  session(): Promise<SessionInfo>;
  sendUserMessage(text: string): Promise<void>;
  snapshot(): Promise<Snapshot>;
  config(): Promise<any>;
  registerProvider(provider: any): Promise<void>;
  providerEvent(provider: string, kind: ProviderEventKind, message: string, detail?: any): Promise<void>;
  widgetUpdate(update: WidgetUpdateParams): Promise<void>;
}

/** The event sink scoped to one `provider/stream` call: sends
 * `provider/streamEvent` notifications (AssistantMessageEvent-shaped
 * plain objects) and enforces exactly one terminal event (done/error). */
export interface ProviderEvents {
  readonly streamId: string;
  /** Send one event. Terminal events (`done`/`error`) may be sent
   * exactly once; a second one rejects. */
  send(event: any): Promise<void>;
  /** `textDelta` convenience (`partial` is the accumulated message). */
  textDelta(contentIndex: number, delta: string, partial: any): Promise<void>;
  /** `thinkingDelta` convenience (`partial` is the accumulated message). */
  thinkingDelta(contentIndex: number, delta: string, partial: any): Promise<void>;
  /** Terminal `done` event; `reason` defaults to the message's
   * `stopReason` (or `stop`). */
  done(message: any): Promise<void>;
  /** Terminal `error` event. `message` defaults to a zeroed assistant
   * message built from the served model carrying `errorMessage`. */
  error(errorMessage: string, message?: any): Promise<void>;
}

/** Stream-scoped context for a `provider/stream` handler: the plugin's
 * host context plus the stream's cancellation signal. */
export interface ProviderStreamCx {
  /** The plugin's host context (mode, trust, config, host client). */
  cx: Cx;
  /** This stream's id. */
  streamId: string;
  /** Poll the cancellation signal. */
  isCancelled(): boolean;
  /** Resolve when the host cancels the stream (`provider/streamCancel`). */
  cancelled(): Promise<void>;
}

type Async<F> = F | Promise<F>;

export interface PluginBuilder {
  tool(spec: ToolSpec, handler: (params: ToolExecuteParams, cx: Cx) => Async<ToolOutput>): this;
  command(
    name: string,
    description: string | undefined,
    handler: (params: CommandInvokeParams, cx: Cx) => Async<any>,
  ): this;
  beforeToolCall(handler: (params: BeforeToolCallParams, cx: Cx) => Async<Verdict>): this;
  afterToolCall(
    handler: (params: AfterToolCallParams, cx: Cx) => Async<AfterToolCallPatch | null>,
  ): this;
  transformContext(
    handler: (params: TransformContextParams, cx: Cx) => Async<TransformContextResult | null>,
  ): this;
  approvalReview(
    handler: (params: ApprovalReviewParams, cx: Cx) => Async<ApprovalDecision | null>,
  ): this;
  events(names: string[], handler: (params: LifecycleEventParams, cx: Cx) => Async<void>): this;
  widget(spec: WidgetSpec): this;
  onWidgetAction(handler: (params: WidgetActionParams, cx: Cx) => Async<void>): this;
  autocomplete(
    spec: AutocompleteProviderSpec,
    handler: (params: AutocompleteProvideParams, cx: Cx) => Async<AutocompleteProvideResult>,
  ): this;
  configSchema(schema: object): this;
  metrics(declaration: MetricsDeclaration): this;
  /** Serve inference for registered providers (the P7 provider bridge):
   * declares the `provider.stream` capability. The host calls
   * `provider/stream` for every turn on the models of providers this
   * plugin registered with `bridge: true`; events flow back through
   * `events`, cancellation surfaces on the stream context. */
  providerStream(
    handler: (params: ProviderStreamParams, events: ProviderEvents, cx: ProviderStreamCx) => Async<void>,
  ): this;
  /** Declare the `provider.register` capability: the plugin calls
   * `cx.host.registerProvider(...)` with plain (non-bridge) provider
   * specs (typically from `onReady`). The host rejects plain
   * registrations from plugins that did not declare it; bridge
   * providers (`bridge: true`) serve inference and need
   * `providerStream` instead. */
  providerRegister(enabled?: boolean): this;
  /** Fired once after the initialize handshake is answered. The
   * registration entry point for provider plugins (call
   * `cx.host.registerProvider(...)` here). */
  onReady(handler: (cx: Cx) => Async<void>): this;
  run(options?: {
    input?: AsyncIterable<any>;
    output?: { write(s: string, cb?: (err?: Error) => void): unknown };
  }): Promise<void>;
}

export declare function plugin(options: {
  name: string;
  version?: string;
  description?: string;
}): PluginBuilder;

export declare function textBlock(text: string): { type: "text"; text: string };
export declare function textOutput(text: string): ToolOutput;
export declare function errorOutput(text: string): ToolOutput;
export declare function allow(): Verdict;
export declare function deny(reason: string): Verdict;
export declare function rewrite(args: any): Verdict;
