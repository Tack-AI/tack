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
  widgetUpdate(update: WidgetUpdateParams): Promise<void>;
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
