import { useTranslation } from "react-i18next";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useProvidersQuery } from "@/lib/query/queries";
import {
  useClearProjectRoute,
  useProjectRoutingQuery,
  useSetProjectRoute,
} from "@/lib/query/projectRouting";
import type { Provider } from "@/types";
import type { ProjectRouteInfo } from "@/types/projectRouting";
import { formatRelativeTime } from "@/components/sessions/utils";
import { cn } from "@/lib/utils";
import {
  providerOptionKind,
  syncBadgeTone,
  type SyncBadgeTone,
} from "./syncStatus";

// ============================================================================
// 项目路由页面：项目列表 + 供应商绑定 + 同步状态徽标
//
// 数据：useProjectRoutingQuery(true) 挂载即 3s 轮询（卸载由 query enabled 停）
// 交互契约（spec §7.1.3 真值表）：
//   out_of_sync  → 警示 + 一键修复（setProjectRoute 按 DB 重写文件）
//   orphan_header → 提示（header 不参与路由）+ 可选清除（clearProjectRoute）
// ============================================================================

/** "默认（不绑定）"选项哨兵值——供应商 id 均为 UUID，无碰撞风险 */
const DEFAULT_PROVIDER_VALUE = "__default__";

/** 同步徽标 tone → 色点类（ok 绿 / warn 黄 / info 蓝，语义对齐 failover 状态点） */
const TONE_DOT_CLASS: Record<SyncBadgeTone, string> = {
  ok: "bg-emerald-500",
  warn: "bg-amber-500",
  info: "bg-sky-500",
};

interface ProjectRouteRowProps {
  info: ProjectRouteInfo;
  providerList: Provider[];
  pending: boolean;
  onSelect: (info: ProjectRouteInfo, value: string) => void;
  onFix: (info: ProjectRouteInfo) => void;
  onClear: (info: ProjectRouteInfo) => void;
}

/** 单行：名称 / 活跃时间 / 绑定 Select / 同步徽标——其余为声明式 JSX */
function ProjectRouteRow({
  info,
  providerList,
  pending,
  onSelect,
  onFix,
  onClear,
}: ProjectRouteRowProps) {
  const { t } = useTranslation();
  const tone = syncBadgeTone(info.syncStatus);
  const optionKind = providerOptionKind(info);
  // 失效绑定：供应商已删不在选项中 → Select 无匹配项回落 placeholder，
  // 展示"名称 · 已失效"（不静默清空，spec §8 边界 9）
  const invalidPlaceholder = info.boundProviderName
    ? `${info.boundProviderName} · ${t("projectRouting.invalid")}`
    : t("projectRouting.invalid");

  return (
    <TableRow>
      {/* 项目：basename 主展示 + 完整路径 tooltip + 使用中徽标 */}
      <TableCell className="max-w-[320px]">
        <div className="flex items-center gap-2 min-w-0">
          <span
            className="font-medium truncate"
            title={info.projectPath}
          >
            {info.basename}
          </span>
          {info.hasActiveSession && (
            <Badge
              variant="secondary"
              className="h-5 px-1.5 text-[10px] whitespace-nowrap"
            >
              {t("projectRouting.inUse")}
            </Badge>
          )}
        </div>
      </TableCell>

      {/* 活跃时间：相对时间，悬浮显示绝对时间（后端为 Unix 秒） */}
      <TableCell className="text-xs text-muted-foreground whitespace-nowrap">
        <span
          title={new Date(info.lastActiveAt * 1000).toLocaleString()}
        >
          {formatRelativeTime(info.lastActiveAt * 1000, t)}
        </span>
      </TableCell>

      {/* 绑定供应商：默认（不绑定）+ claude 供应商列表 */}
      <TableCell>
        <Select
          value={info.boundProviderId ?? DEFAULT_PROVIDER_VALUE}
          onValueChange={(value) => onSelect(info, value)}
          disabled={pending}
        >
          <SelectTrigger className="h-8 w-[200px] bg-background text-xs">
            {/* 失效绑定（供应商已删）不在选项中 → Radix 无匹配项渲染空白，
                需显式传 children 展示"名称 · 已失效"，不静默清空（spec §8 边界 9） */}
            <SelectValue>
              {optionKind === "invalid" ? invalidPlaceholder : undefined}
            </SelectValue>
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={DEFAULT_PROVIDER_VALUE}>
              {t("projectRouting.default")}
            </SelectItem>
            {providerList.map((provider) => (
              <SelectItem key={provider.id} value={provider.id}>
                <span className="block truncate" title={provider.name}>
                  {provider.name}
                </span>
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </TableCell>

      {/* 同步状态：色点 + 文案 + 真值表对应的修复/清除动作 */}
      <TableCell>
        <div className="flex items-center gap-2">
          <span
            className={cn(
              "w-2 h-2 rounded-full flex-shrink-0",
              TONE_DOT_CLASS[tone],
            )}
          />
          <span className="text-xs text-muted-foreground whitespace-nowrap">
            {info.syncStatus === "synced" && t("projectRouting.sync.synced")}
            {info.syncStatus === "out_of_sync" &&
              t("projectRouting.sync.outOfSync")}
            {info.syncStatus === "orphan_header" &&
              t("projectRouting.sync.orphan")}
          </span>
          {info.syncStatus === "out_of_sync" && info.boundProviderId && (
            <Button
              variant="outline"
              size="sm"
              className="h-6 px-2 text-xs"
              disabled={pending}
              onClick={() => onFix(info)}
            >
              {t("projectRouting.sync.fix")}
            </Button>
          )}
          {info.syncStatus === "orphan_header" && (
            <Button
              variant="outline"
              size="sm"
              className="h-6 px-2 text-xs"
              disabled={pending}
              onClick={() => onClear(info)}
            >
              {t("projectRouting.sync.clear")}
            </Button>
          )}
        </div>
      </TableCell>
    </TableRow>
  );
}

export function ProjectRoutingPage() {
  const { t } = useTranslation();

  // 页面挂载即轮询（3s），组件卸载后 query enabled 机制自动停止
  const { data: projects, isLoading } = useProjectRoutingQuery(true);
  // 供应商下拉数据源：复用现有 claude 供应商查询，不新增 API 封装
  const { data: providersData } = useProvidersQuery("claude");
  const setRoute = useSetProjectRoute();
  const clearRoute = useClearProjectRoute();

  const providerList = Object.values(providersData?.providers ?? {}).sort(
    (a, b) => a.name.localeCompare(b.name, "zh-CN"),
  );
  const pending = setRoute.isPending || clearRoute.isPending;

  // Select 变更：选"默认"即解除绑定，否则建立绑定
  const handleSelect = (info: ProjectRouteInfo, value: string) => {
    if (value === DEFAULT_PROVIDER_VALUE) {
      clearRoute.mutate(info.projectPath);
    } else {
      setRoute.mutate({ projectPath: info.projectPath, providerId: value });
    }
  };

  // 一键修复：按 DB 绑定重写 settings.local.json（保留用户其他内容）
  const handleFix = (info: ProjectRouteInfo) => {
    if (info.boundProviderId) {
      setRoute.mutate({
        projectPath: info.projectPath,
        providerId: info.boundProviderId,
      });
    }
  };

  // 清除孤儿行：header 不参与路由，删除仅消除提示（不自动删，用户主动触发）
  const handleClear = (info: ProjectRouteInfo) => {
    clearRoute.mutate(info.projectPath);
  };

  return (
    <div className="px-6 pt-4 pb-12">
      <p className="text-xs text-muted-foreground mb-3">
        {t("projectRouting.subtitle")}
      </p>
      {isLoading ? (
        <div className="h-[300px] animate-pulse rounded-lg bg-muted/50" />
      ) : (projects?.length ?? 0) === 0 ? (
        <div className="rounded-lg border bg-card/50 p-8 text-center text-sm text-muted-foreground">
          {t("projectRouting.empty")}
        </div>
      ) : (
        <div className="rounded-lg border bg-card/50 backdrop-blur-sm overflow-x-auto">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{t("projectRouting.column.project")}</TableHead>
                <TableHead>{t("projectRouting.column.activity")}</TableHead>
                <TableHead>{t("projectRouting.column.provider")}</TableHead>
                <TableHead>{t("projectRouting.column.sync")}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(projects ?? []).map((info) => (
                <ProjectRouteRow
                  key={info.projectPath}
                  info={info}
                  providerList={providerList}
                  pending={pending}
                  onSelect={handleSelect}
                  onFix={handleFix}
                  onClear={handleClear}
                />
              ))}
            </TableBody>
          </Table>
        </div>
      )}
    </div>
  );
}
