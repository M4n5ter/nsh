export type Section = "services" | "incidents" | "releases";
export type Health = "healthy" | "degraded";
export type Filter = "all" | Health;

export interface Service {
  id: number;
  name: string;
  group: string;
  region: string;
  location: string;
  owner: string;
  health: Health;
  latency: number;
  cpu: number;
  memory: number;
  replicas: number;
  version: string;
  updated: string;
}

export interface Activity {
  time: string;
  title: string;
  detail: string;
  tone: "mint" | "amber" | "blue";
}

const definitions = [
  ["api-gateway", "Edge / routing", "us-east-1", "Virginia", "Alex Lee · Platform"],
  ["inference-worker", "AI / inference", "ap-northeast-1", "Tokyo · 東京", "王宁 · AI Runtime"],
  ["billing-service", "Payments / core", "eu-west-1", "Dublin", "Julia Mora · Billing"],
  ["identity-service", "Security / auth", "us-east-1", "Virginia", "Sam Okafor · Identity"],
  ["event-router", "Messaging / events", "eu-central-1", "Frankfurt", "Alex Lee · Platform"],
  ["search-indexer", "Search / indexing", "eu-west-1", "Dublin", "Mina Park · Search"],
  ["edge-gateway", "Edge / CDN", "ap-southeast-1", "Singapore", "陈晨 · Edge"],
  ["notifications", "Messaging / delivery", "us-west-2", "Oregon", "Julia Mora · Delivery"],
  ["media-pipeline", "Media / encoding", "us-west-2", "Oregon", "Owen Chen · Media"],
  ["scheduler", "Jobs / scheduling", "us-east-1", "Virginia", "Sam Okafor · Platform"],
  ["analytics-store", "Data / warehouse", "eu-central-1", "Frankfurt", "Mina Park · Data"],
  ["unicode-console", "Tools / developer", "ap-northeast-1", "Tokyo · 東京", "平台团队 · 王宁"],
] as const;

export const services: Service[] = Array.from({ length: 72 }, (_, index) => {
  const definition = definitions[index % definitions.length];
  const degraded = index === 1 || index === 5 || index === 8 || (index > 11 && index % 17 === 0);
  return {
    id: index + 1,
    name: index < definitions.length ? definition[0] : `worker-pool-${String(index + 1).padStart(2, "0")}`,
    group: index < definitions.length ? definition[1] : "Compute / workers",
    region: definition[2],
    location: definition[3],
    owner: definition[4],
    health: degraded ? "degraded" : "healthy",
    latency: index === 1 ? 486 : degraded ? 230 + index * 7 : 24 + (index * 13) % 94,
    cpu: index === 1 ? 84 : degraded ? 76 + index % 19 : 22 + (index * 7) % 45,
    memory: index === 1 ? 76 : 32 + (index * 11) % 51,
    replicas: index < 3 ? 12 : 3 + index % 6,
    version: `v2.18.${4 - index % 5}`,
    updated: index < 8 ? `${2 + index * 3}m ago` : `${1 + index % 8}h ago`,
  };
});

export const initialActivity: Activity[] = [
  { time: "14:32", title: "Latency alert", detail: "inference-worker · 486ms", tone: "amber" },
  { time: "14:29", title: "Canary passed", detail: "api-gateway · v2.18.4", tone: "mint" },
  { time: "14:24", title: "王宁 acknowledged", detail: "Tokyo inference cluster", tone: "blue" },
];

export const sections: { id: Section; label: string; icon: string }[] = [
  { id: "services", label: "Services", icon: "▦" },
  { id: "incidents", label: "Incidents", icon: "!" },
  { id: "releases", label: "Releases", icon: "↗" },
];
