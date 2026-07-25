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
  // Plugin catalog (Wave 1) — hex colors chosen per plugin.
  'request-id':         { color: '#0ea5e9', icon: Fingerprint },
  'real-ip':            { color: '#d946ef', icon: Network },
  redirect:             { color: '#eab308', icon: CornerUpRight },
  echo:                 { color: '#22c55e', icon: MessageSquareQuote },
  'ua-restriction':     { color: '#d946ef', icon: Fingerprint },
  'referer-restriction': { color: '#f43f5e', icon: Link2Off },
  'uri-blocker':        { color: '#dc2626', icon: Ban },
  csrf:                 { color: '#6366f1', icon: ShieldCheck },
  'response-rewrite':   { color: '#6366f1', icon: Replace },
  gzip:                 { color: '#0ea5e9', icon: FileArchive },
  brotli:               { color: '#f43f5e', icon: Archive },
  'error-page':         { color: '#dc2626', icon: FileWarning },
  'exit-transformer':   { color: '#7c3aed', icon: DoorOpen },
  'fault-injection':    { color: '#ef4444', icon: Zap },
  workflow:             { color: '#8b5cf6', icon: Workflow },
  'traffic-label':      { color: '#14b8a6', icon: Tags },
  mocking:              { color: '#f59e0b', icon: FlaskConical },
  'data-mask':          { color: '#64748b', icon: EyeOff },
  'request-validation': { color: '#eab308', icon: FileCheck },
  'body-transformer':   { color: '#22c55e', icon: Replace },
  degraphql:            { color: '#d946ef', icon: Waypoints },
  // Wave 2 — consumer/auth core.
  'hmac-auth':          { color: '#14b8a6', icon: FileSignature },
  'multi-auth':         { color: '#0ea5e9', icon: ShieldCheck },
  'jwe-decrypt':        { color: '#8b5cf6', icon: LockKeyholeOpen },
  'consumer-restriction': { color: '#f97316', icon: UserCheck },
  acl:                  { color: '#f59e0b', icon: ShieldCheck },
  'attach-consumer-label': { color: '#eab308', icon: Tags },
  // Wave 3 — callout auth & authz.
  'forward-auth':       { color: '#0ea5e9', icon: ShieldCheck },
  opa:                  { color: '#7c3aed', icon: Scale },
  'authz-casbin':       { color: '#7c3aed', icon: ScrollText },
  'authz-keycloak':     { color: '#0ea5e9', icon: KeyRound },
  'authz-casdoor':      { color: '#f59e0b', icon: Fingerprint },
  'ldap-auth':          { color: '#0ea5e9', icon: Network },
  'wolf-rbac':          { color: '#f97316', icon: ShieldCheck },
  'cas-auth':           { color: '#8b5cf6', icon: Ticket },
  'openid-connect':     { color: '#6d28d9', icon: ShieldCheck },
  'dingtalk-auth':      { color: '#1493ff', icon: UserCheck },
  'feishu-auth':        { color: '#00c2a8', icon: BadgeCheck },
  // Wave 4 — traffic control.
  'limit-count':        { color: '#ef4444', icon: Gauge },
  'proxy-mirror':       { color: '#8b5cf6', icon: Copy },
  'traffic-split':      { color: '#14b8a6', icon: Split },
  'limit-conn':         { color: '#0ea5e9', icon: Gauge },
  'api-breaker':        { color: '#ef4444', icon: ZapOff },
  'proxy-cache':        { color: '#8b5cf6', icon: Database },
  // Wave 5 — loggers.
  'http-logger':        { color: '#0ea5e9', icon: Webhook },
  'loki-logger':        { color: '#f97316', icon: ScrollText },
  'splunk-hec-logging': { color: '#65a30d', icon: Radio },
  datadog:              { color: '#7c3aed', icon: Dog },
  loggly:               { color: '#dc2626', icon: Tags },
  'elasticsearch-logger': { color: '#14b8a6', icon: Search },
  'clickhouse-logger':  { color: '#eab308', icon: Database },
  'sls-logger':         { color: '#f97316', icon: ScrollText },
  'tencent-cloud-cls':  { color: '#3b82f6', icon: Cloud },
  'tcp-logger':         { color: '#0ea5e9', icon: Network },
  'udp-logger':         { color: '#22c55e', icon: Radio },
  syslog:               { color: '#a855f7', icon: ScrollText },
  'file-logger':        { color: '#f59e0b', icon: FileText },
  'error-log-logger':   { color: '#ef4444', icon: TriangleAlert },
  'google-cloud-logging': { color: '#4285f4', icon: Cloud },
  'skywalking-logger':  { color: '#ff6d00', icon: Telescope },
  lago:                 { color: '#6c5ce7', icon: Receipt },
  // Wave 6 — tracing & metrics.
  opentelemetry:        { color: '#425cc7', icon: Waypoints },
  zipkin:               { color: '#ff9a3c', icon: Network },
  skywalking:           { color: '#5b3fa8', icon: Telescope },
  prometheus:           { color: '#e6522c', icon: Gauge },
  // Wave 7 — serverless & FaaS.
  'serverless-pre-function':  { color: '#06b6d4', icon: Code },
  'serverless-post-function': { color: '#06b6d4', icon: Code },
  'oas-validator':      { color: '#10b981', icon: FileCheck },
  'aws-lambda':         { color: '#ff9900', icon: Cloud },
  'azure-functions':    { color: '#0062ad', icon: Cloud },
  openwhisk:            { color: '#3c873a', icon: Zap },
  openfunction:         { color: '#00b4a0', icon: Boxes },
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
