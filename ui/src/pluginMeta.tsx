/**
 * Visual identity registry for plugin types — maps each node/plugin type
 * to its CSS color token and Lucide icon, used everywhere a node is drawn
 * (canvas, palette, sidebar). Mirrors the featherbit design system's
 * pluginMeta map.
 *
 * @module pluginMeta
 */
import type { LucideIcon } from 'lucide-react';
import {
  Variable,
  Archive,
  ArrowLeftRight,
  BadgeCheck,
  Ban,
  Box,
  Boxes,
  Braces,
  Cloud,
  Code,
  CornerUpRight,
  Copy,
  Database,
  Dog,
  DoorOpen,
  EyeOff,
  FileArchive,
  FileCheck,
  FileSignature,
  FileText,
  FileWarning,
  Fingerprint,
  FlaskConical,
  GitBranch,
  Gauge,
  Globe,
  KeyRound,
  Link2Off,
  LockKeyholeOpen,
  LogIn,
  LogOut,
  MessageSquareQuote,
  Network,
  Radio,
  Receipt,
  Replace,
  Ruler,
  Scale,
  ScrollText,
  Search,
  Server,
  Shield,
  ShieldCheck,
  Split,
  Tags,
  Telescope,
  Ticket,
  TriangleAlert,
  UserCheck,
  UserRound,
  Waypoints,
  Webhook,
  Workflow,
  Zap,
  ZapOff,
} from 'lucide-react';

/**
 * Visual identity of a plugin type.
 */
export interface PluginMeta {
  /** CSS custom-property token (e.g. `var(--plugin-upstream)`) used as the node's accent color. */
  color: string;
  /** Lucide icon component rendered on the node. */
  icon: LucideIcon;
}

/**
 * Canonical per-plugin identity — color token + Lucide icon.
 * Mirrors the featherbit design system's pluginMeta map.
 *
 * @remarks
 * Keys match the plugin type names registered by create_plugin in
 * src/plugins/mod.rs, plus the pseudo node types `listener` and `client`.
 * The three auth plugins deliberately share one color token.
 */
export const pluginMeta: Record<string, PluginMeta> = {
  listener:             { color: 'var(--plugin-listener)',  icon: LogIn },
  client:               { color: 'var(--plugin-client)',    icon: LogOut },
  'proxy-rewrite':      { color: 'var(--plugin-proxy)',     icon: ArrowLeftRight },
  upstream:             { color: 'var(--plugin-upstream)',  icon: Server },
  'error-handler':      { color: 'var(--plugin-error)',     icon: TriangleAlert },
  cors:                 { color: 'var(--plugin-cors)',      icon: Globe },
  'rate-limit':         { color: 'var(--plugin-ratelimit)', icon: Gauge },
  'ip-restriction':     { color: 'var(--plugin-ip)',        icon: Shield },
  'request-size-limit': { color: 'var(--plugin-size)',      icon: Ruler },
  'key-auth':           { color: 'var(--plugin-auth)',      icon: KeyRound },
  'basic-auth':         { color: 'var(--plugin-auth)',      icon: UserRound },
  'jwt-auth':           { color: 'var(--plugin-auth)',      icon: BadgeCheck },
  logging:              { color: 'var(--plugin-logging)',   icon: ScrollText },
  script:               { color: 'var(--plugin-script)',    icon: Braces },
  // Plugin catalog (Wave 1). Catalog colors keep a per-plugin hue at the
  // shared --plugin-l / --plugin-c lightness and chroma (set per theme).
  'request-id':         { color: 'oklch(var(--plugin-l) var(--plugin-c) 237)', icon: Fingerprint },
  'real-ip':            { color: 'oklch(var(--plugin-l) var(--plugin-c) 322)', icon: Network },
  redirect:             { color: 'oklch(var(--plugin-l) var(--plugin-c) 86)', icon: CornerUpRight },
  echo:                 { color: 'oklch(var(--plugin-l) var(--plugin-c) 150)', icon: MessageSquareQuote },
  'ua-restriction':     { color: 'oklch(var(--plugin-l) var(--plugin-c) 322)', icon: Fingerprint },
  'referer-restriction': { color: 'oklch(var(--plugin-l) var(--plugin-c) 16)', icon: Link2Off },
  'uri-blocker':        { color: 'oklch(var(--plugin-l) var(--plugin-c) 27)', icon: Ban },
  csrf:                 { color: 'oklch(var(--plugin-l) var(--plugin-c) 277)', icon: ShieldCheck },
  'response-rewrite':   { color: 'oklch(var(--plugin-l) var(--plugin-c) 277)', icon: Replace },
  gzip:                 { color: 'oklch(var(--plugin-l) var(--plugin-c) 237)', icon: FileArchive },
  brotli:               { color: 'oklch(var(--plugin-l) var(--plugin-c) 16)', icon: Archive },
  'error-page':         { color: 'oklch(var(--plugin-l) var(--plugin-c) 27)', icon: FileWarning },
  'exit-transformer':   { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: DoorOpen },
  'fault-injection':    { color: 'oklch(var(--plugin-l) var(--plugin-c) 25)', icon: Zap },
  workflow:             { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: Workflow },
  condition:            { color: 'oklch(var(--plugin-l) var(--plugin-c) 70)', icon: GitBranch },
  'traffic-label':      { color: 'oklch(var(--plugin-l) var(--plugin-c) 183)', icon: Tags },
  'set-vars':           { color: 'oklch(var(--plugin-l) var(--plugin-c) 237)', icon: Variable },
  mocking:              { color: 'oklch(var(--plugin-l) var(--plugin-c) 70)', icon: FlaskConical },
  'data-mask':          { color: 'var(--plugin-logging)', icon: EyeOff },
  'request-validation': { color: 'oklch(var(--plugin-l) var(--plugin-c) 86)', icon: FileCheck },
  'body-transformer':   { color: 'oklch(var(--plugin-l) var(--plugin-c) 150)', icon: Replace },
  degraphql:            { color: 'oklch(var(--plugin-l) var(--plugin-c) 322)', icon: Waypoints },
  // Wave 2 — consumer/auth core.
  'hmac-auth':          { color: 'oklch(var(--plugin-l) var(--plugin-c) 183)', icon: FileSignature },
  'multi-auth':         { color: 'oklch(var(--plugin-l) var(--plugin-c) 237)', icon: ShieldCheck },
  'jwe-decrypt':        { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: LockKeyholeOpen },
  'consumer-restriction': { color: 'oklch(var(--plugin-l) var(--plugin-c) 48)', icon: UserCheck },
  acl:                  { color: 'oklch(var(--plugin-l) var(--plugin-c) 70)', icon: ShieldCheck },
  'attach-consumer-label': { color: 'oklch(var(--plugin-l) var(--plugin-c) 86)', icon: Tags },
  // Wave 3 — callout auth & authz.
  'forward-auth':       { color: 'oklch(var(--plugin-l) var(--plugin-c) 237)', icon: ShieldCheck },
  opa:                  { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: Scale },
  'authz-casbin':       { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: ScrollText },
  'authz-keycloak':     { color: 'oklch(var(--plugin-l) var(--plugin-c) 237)', icon: KeyRound },
  'authz-casdoor':      { color: 'oklch(var(--plugin-l) var(--plugin-c) 70)', icon: Fingerprint },
  'ldap-auth':          { color: 'oklch(var(--plugin-l) var(--plugin-c) 237)', icon: Network },
  'wolf-rbac':          { color: 'oklch(var(--plugin-l) var(--plugin-c) 48)', icon: ShieldCheck },
  'cas-auth':           { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: Ticket },
  'openid-connect':     { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: ShieldCheck },
  'dingtalk-auth':      { color: 'oklch(var(--plugin-l) var(--plugin-c) 252)', icon: UserCheck },
  'feishu-auth':        { color: 'oklch(var(--plugin-l) var(--plugin-c) 178)', icon: BadgeCheck },
  // Wave 4 — traffic control.
  'limit-count':        { color: 'oklch(var(--plugin-l) var(--plugin-c) 25)', icon: Gauge },
  'proxy-mirror':       { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: Copy },
  'traffic-split':      { color: 'oklch(var(--plugin-l) var(--plugin-c) 183)', icon: Split },
  'limit-conn':         { color: 'oklch(var(--plugin-l) var(--plugin-c) 237)', icon: Gauge },
  'api-breaker':        { color: 'oklch(var(--plugin-l) var(--plugin-c) 25)', icon: ZapOff },
  'proxy-cache':        { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: Database },
  // Wave 5 — loggers.
  'http-logger':        { color: 'oklch(var(--plugin-l) var(--plugin-c) 237)', icon: Webhook },
  'loki-logger':        { color: 'oklch(var(--plugin-l) var(--plugin-c) 48)', icon: ScrollText },
  'splunk-hec-logging': { color: 'oklch(var(--plugin-l) var(--plugin-c) 132)', icon: Radio },
  datadog:              { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: Dog },
  loggly:               { color: 'oklch(var(--plugin-l) var(--plugin-c) 27)', icon: Tags },
  'elasticsearch-logger': { color: 'oklch(var(--plugin-l) var(--plugin-c) 183)', icon: Search },
  'clickhouse-logger':  { color: 'oklch(var(--plugin-l) var(--plugin-c) 86)', icon: Database },
  'sls-logger':         { color: 'oklch(var(--plugin-l) var(--plugin-c) 48)', icon: ScrollText },
  'tencent-cloud-cls':  { color: 'oklch(var(--plugin-l) var(--plugin-c) 260)', icon: Cloud },
  'tcp-logger':         { color: 'oklch(var(--plugin-l) var(--plugin-c) 237)', icon: Network },
  'udp-logger':         { color: 'oklch(var(--plugin-l) var(--plugin-c) 150)', icon: Radio },
  syslog:               { color: 'oklch(var(--plugin-l) var(--plugin-c) 304)', icon: ScrollText },
  'file-logger':        { color: 'oklch(var(--plugin-l) var(--plugin-c) 70)', icon: FileText },
  'error-log-logger':   { color: 'oklch(var(--plugin-l) var(--plugin-c) 25)', icon: TriangleAlert },
  'google-cloud-logging': { color: 'oklch(var(--plugin-l) var(--plugin-c) 260)', icon: Cloud },
  'skywalking-logger':  { color: 'oklch(var(--plugin-l) var(--plugin-c) 46)', icon: Telescope },
  lago:                 { color: 'oklch(var(--plugin-l) var(--plugin-c) 283)', icon: Receipt },
  // Wave 6 — tracing & metrics.
  opentelemetry:        { color: 'oklch(var(--plugin-l) var(--plugin-c) 269)', icon: Waypoints },
  zipkin:               { color: 'oklch(var(--plugin-l) var(--plugin-c) 59)', icon: Network },
  skywalking:           { color: 'oklch(var(--plugin-l) var(--plugin-c) 291)', icon: Telescope },
  prometheus:           { color: 'oklch(var(--plugin-l) var(--plugin-c) 35)', icon: Gauge },
  // Wave 7 — serverless & FaaS.
  'serverless-pre-function':  { color: 'oklch(var(--plugin-l) var(--plugin-c) 215)', icon: Code },
  'serverless-post-function': { color: 'oklch(var(--plugin-l) var(--plugin-c) 215)', icon: Code },
  'oas-validator':      { color: 'oklch(var(--plugin-l) var(--plugin-c) 162)', icon: FileCheck },
  'aws-lambda':         { color: 'oklch(var(--plugin-l) var(--plugin-c) 65)', icon: Cloud },
  'azure-functions':    { color: 'oklch(var(--plugin-l) var(--plugin-c) 251)', icon: Cloud },
  openwhisk:            { color: 'oklch(var(--plugin-l) var(--plugin-c) 143)', icon: Zap },
  openfunction:         { color: 'oklch(var(--plugin-l) var(--plugin-c) 181)', icon: Boxes },
  // Wave 8 — policy state.
  'store-get':          { color: 'oklch(var(--plugin-l) var(--plugin-c) 222)', icon: Database },
  'store-set':          { color: 'oklch(var(--plugin-l) var(--plugin-c) 222)', icon: Database },
  'store-delete':       { color: 'oklch(var(--plugin-l) var(--plugin-c) 222)', icon: Database },
  'store-incr':         { color: 'oklch(var(--plugin-l) var(--plugin-c) 222)', icon: Database },
  // Supernodes and their boundary pseudo-nodes (src/graph/expand.rs)
  supernode:            { color: 'oklch(var(--plugin-l) var(--plugin-c) 293)', icon: Boxes },
  input:                { color: 'var(--plugin-logging)', icon: LogIn },
  output:               { color: 'var(--plugin-logging)', icon: LogOut },
  error:                { color: 'oklch(var(--plugin-l) var(--plugin-c) 28)', icon: TriangleAlert },
};

/**
 * Looks up the visual identity for a plugin type.
 *
 * @param type - Plugin type name (a {@link pluginMeta} key).
 * @returns The registered identity, or a neutral fallback (logging color,
 * generic Box icon) for unknown types.
 */
export function getPluginMeta(type: string): PluginMeta {
  return pluginMeta[type] || { color: 'var(--plugin-logging)', icon: Box };
}
