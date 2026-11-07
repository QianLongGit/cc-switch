import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { Check, ChevronDown, Monitor, Server } from "lucide-react";
import { sessionsApi } from "@/lib/api";
import type { SshHostConfig } from "@/types";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { HoverTip } from "@/components/ui/hover-tip";
import { cn } from "@/lib/utils";
import { sessionMenuItemClass } from "./SessionItem";

/** 本机 ssh 二进制装没装在一次运行里不会变，查一次就够 */
const SSH_AVAILABILITY_KEY = ["sshAvailability"] as const;

export interface SessionHostMenuProps {
  /** 当前选中的主机 id；null = 本机 */
  hostId: string | null;
  /** 设置里保存的 SSH 主机；一条都没有时整个控件不渲染（只有本机，没有切换的意义） */
  sshHosts: SshHostConfig[] | undefined;
  /** 选择主机；null = 切回本机 */
  onSelect: (hostId: string | null) => void;
}

/** 主机切换下拉：结构与样式跟随会话列表的应用筛选下拉。 */
export function SessionHostMenu({
  hostId,
  sshHosts,
  onSelect,
}: SessionHostMenuProps) {
  const { t } = useTranslation();
  const hosts = sshHosts ?? [];
  const availability = useQuery({
    queryKey: SSH_AVAILABILITY_KEY,
    queryFn: () => sessionsApi.checkSshAvailability(),
    enabled: hosts.length > 0,
    staleTime: Infinity,
  });

  if (hosts.length === 0) return null;

  // 结果没回来前（undefined）不禁用，查到 false 才降级
  const sshAvailable = availability.data !== false;
  const unavailableTip = sshAvailable
    ? undefined
    : t("sessionManager.hostSelector.sshUnavailable", {
        defaultValue: "未检测到 ssh，请安装 OpenSSH 客户端",
      });
  const localLabel = t("sessionManager.hostSelector.local", {
    defaultValue: "本机",
  });
  // 选中的主机被从设置里删掉时，trigger 退回显示「本机」
  const currentHost = hostId
    ? (hosts.find((host) => host.id === hostId) ?? null)
    : null;

  return (
    <DropdownMenu>
      <HoverTip content={unavailableTip}>
        {/* span 包一层：disabled 按钮收不到鼠标事件，tooltip 挂在 span 上照样能弹 */}
        <span className="inline-flex shrink-0">
          <DropdownMenuTrigger asChild>
            <Button
              variant="neutral"
              size="regular"
              disabled={!sshAvailable}
              className="gap-2 pe-2 ps-2.5"
            >
              <span className="sr-only">
                {t("sessionManager.hostSelector.choose", {
                  defaultValue: "选择主机",
                })}
              </span>
              {currentHost ? (
                <Server
                  aria-hidden="true"
                  className="h-4 w-4 text-fg-2"
                  strokeWidth={2}
                />
              ) : (
                <Monitor
                  aria-hidden="true"
                  className="h-4 w-4 text-fg-2"
                  strokeWidth={2}
                />
              )}
              <span>{currentHost?.label ?? localLabel}</span>
              <ChevronDown
                aria-hidden="true"
                className="h-3.5 w-3.5 text-fg-2"
                strokeWidth={2}
              />
            </Button>
          </DropdownMenuTrigger>
        </span>
      </HoverTip>
      <DropdownMenuContent
        align="start"
        aria-label={t("sessionManager.hostSelector.choose", {
          defaultValue: "选择主机",
        })}
        className="w-[240px] rounded-panel p-1 shadow-v7-md"
      >
        <HostMenuItem
          pressed={!hostId}
          icon={
            <Monitor className="h-4 w-4 text-fg-2" strokeWidth={1.5} />
          }
          label={localLabel}
          onSelect={() => onSelect(null)}
        />
        <DropdownMenuSeparator />
        {hosts.map((host) => (
          <HostMenuItem
            key={host.id}
            pressed={hostId === host.id}
            icon={
              <Server className="h-4 w-4 text-fg-2" strokeWidth={1.5} />
            }
            label={host.label}
            sub={host.user ? `${host.user}@${host.host}` : host.host}
            onSelect={() => onSelect(host.id)}
          />
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function HostMenuItem({
  pressed,
  icon,
  label,
  sub,
  onSelect,
}: {
  pressed: boolean;
  icon: React.ReactNode;
  label: string;
  /** 副文案：远程主机的主机名（含用户名时拼成 user@host） */
  sub?: string;
  onSelect: () => void;
}) {
  return (
    <DropdownMenuItem
      role="menuitemradio"
      aria-checked={pressed}
      onSelect={onSelect}
      className={cn(
        sessionMenuItemClass,
        "gap-2 pe-2",
        pressed && "font-medium",
      )}
    >
      {icon}
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="min-w-0 truncate">{label}</span>
        {sub && (
          <span className="min-w-0 truncate text-caption font-normal text-fg-3">
            {sub}
          </span>
        )}
      </span>
      <Check
        aria-hidden="true"
        strokeWidth={1.5}
        className={cn("h-3.5 w-3.5 shrink-0", !pressed && "invisible")}
      />
    </DropdownMenuItem>
  );
}
