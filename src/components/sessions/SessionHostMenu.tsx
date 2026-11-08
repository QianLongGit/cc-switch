import { useState } from "react";
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

/**
 * 菜单底部的危险操作入口（如用量页的「重置该主机数据…」）。
 * 仅选中有效远程主机时渲染；label 由调用方传入（i18n 归调用方命名空间）。
 */
export interface SessionHostMenuDangerEntry {
  label: React.ReactNode;
  /** 点击后回传当前选中的主机 id */
  onTrigger: (hostId: string) => void;
  /** 禁用态（如用量页同步进行中：重置与 sync 增量写入并发会互相打架） */
  disabled?: boolean;
}

export interface SessionHostMenuProps {
  /** 当前选中的主机 id；null = 本机 */
  hostId: string | null;
  /** 设置里保存的 SSH 主机；一条都没有时整个控件不渲染（只有本机，没有切换的意义） */
  sshHosts: SshHostConfig[] | undefined;
  /** 选择主机；null = 切回本机 */
  onSelect: (hostId: string | null) => void;
  /** 触发器关闭态的悬停说明（如用量页的「切换统计主机」）；ssh 不可用的禁用说明优先于它 */
  triggerHint?: React.ReactNode;
  /** 危险操作入口（本机态不渲染）；不传时菜单与原先完全一致 */
  dangerEntry?: SessionHostMenuDangerEntry;
}

/** 主机切换下拉：结构与样式跟随会话列表的应用筛选下拉。 */
export function SessionHostMenu({
  hostId,
  sshHosts,
  onSelect,
  triggerHint,
  dangerEntry,
}: SessionHostMenuProps) {
  const { t } = useTranslation();
  // 菜单展开期间不显示悬停提示：提示胶囊（z-130）与下拉列表（z-100）都在触发器
  // 正下方，展开时一次指针微动就会让提示重新弹出盖住列表条目，且胶囊可 hover、
  // 会拦截点击（用户实测反馈）。只观察开合，开关仍交给 Radix 内部管理
  const [menuOpen, setMenuOpen] = useState(false);
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

  // 提示内容：ssh 缺失的禁用说明优先；菜单展开期间整体不渲染（见 menuOpen 注释）。
  // content 为空时 HoverTip 原样渲染子元素，Tooltip 树随之卸载，展开期间无法重弹
  const tipContent = menuOpen ? undefined : (unavailableTip ?? triggerHint);

  return (
    <DropdownMenu onOpenChange={setMenuOpen}>
      <HoverTip content={tipContent}>
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
          icon={<Monitor className="h-4 w-4 text-fg-2" strokeWidth={1.5} />}
          label={localLabel}
          onSelect={() => onSelect(null)}
        />
        <DropdownMenuSeparator />
        {hosts.map((host) => (
          <HostMenuItem
            key={host.id}
            pressed={hostId === host.id}
            icon={<Server className="h-4 w-4 text-fg-2" strokeWidth={1.5} />}
            label={host.label}
            sub={host.user ? `${host.user}@${host.host}` : host.host}
            onSelect={() => onSelect(host.id)}
          />
        ))}
        {/* 危险操作入口：只对有效选中的远程主机渲染（本机 / 已删主机不给），
            分隔线 + danger 文字色与 SessionItem 的删除项同构 */}
        {dangerEntry && currentHost && (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              onSelect={() => dangerEntry.onTrigger(currentHost.id)}
              disabled={dangerEntry.disabled}
              className={cn(
                sessionMenuItemClass,
                "text-danger-text",
                dangerEntry.disabled && "opacity-50",
              )}
            >
              {dangerEntry.label}
            </DropdownMenuItem>
          </>
        )}
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
