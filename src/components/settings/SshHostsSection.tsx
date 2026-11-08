import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import { z } from "zod";
import { Check, Loader2, Pencil, PlugZap, Plus, Trash2, X } from "lucide-react";
import { sessionsApi, usageApi } from "@/lib/api";
import { toast } from "@/lib/toast";
import { generateUUID } from "@/utils/uuid";
import type { SettingsFormState } from "@/hooks/useSettings";
import type { SshConfigHost, SshHostConfig } from "@/types";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Form,
  FormControl,
  FormField,
  FormItem,
  FormLabel,
  FormMessage,
} from "@/components/ui/form";
import { HoverTip } from "@/components/ui/hover-tip";
import { Input } from "@/components/ui/input";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { SettingsBlock, SettingsCard, SettingsRow } from "./SettingsLayout";

// ============================================================================
// SSH 远程主机管理（设置 → SSH 主机）
//
// 数据流：所有增删改都走 onAutoSave({ sshHosts }) 全量提交——后端
// save_settings 是全量替换，payload 由 useSettings 组装时原样透传
// （useSettings.sshHosts.test.tsx 守护），这里只负责维护列表本身。
//
// id 约束（Task 7 审查交接）：ssh: source_path 以 id 拼段，含 "/" 的 id
// 会破坏往返。新增 id 一律 generateUUID()（天然无 "/"）；schema 与提交
// 前双重拒绝坏 id（编辑 / 导入路径带入的旧数据同样拦截）。
// ============================================================================

// ─── 校验 schema（手动添加 / 编辑共用） ────────────────────────────────────

/** 表单里的 port 允许字符串（input 原始值）或数字；空串 / 纯空白归一为未填 */
const portValue = z
  .union([z.number(), z.string().trim()])
  .optional()
  .transform((value) => {
    if (value === undefined || value === "") return undefined;
    return typeof value === "number" ? value : Number(value);
  })
  .pipe(z.number().int().min(1).max(65535).optional());

export const sshHostFormSchema = z.object({
  // crypto.randomUUID 生成天然无 "/"；refine 是防御旧数据 / 手改配置文件
  id: z.string().refine((value) => !value.includes("/"), {
    message: "主机 ID 含非法字符 “/”，无法保存",
  }),
  label: z.string().trim().min(1, { message: "请填写名称" }),
  host: z.string().trim().min(1, { message: "请填写主机地址" }),
  user: z.string().trim().optional(),
  port: portValue,
  identityFile: z.string().trim().optional(),
});

/** 表单原始值（port 尚为字符串） */
export type SshHostFormValues = z.input<typeof sshHostFormSchema>;
/** 校验后的配置草稿（port 已归一为 1-65535 的整数或缺省） */
export type SshHostConfigDraft = z.output<typeof sshHostFormSchema>;

// ─── 导入去重：host + (port ?? 22) + (user ?? "") 归一匹配 ─────────────────

export const sshHostMatchKey = (
  host: string,
  port?: number,
  user?: string,
): string =>
  [
    // host 走 DNS，大小写不敏感；user 是登录身份，大小写敏感——只 trim 不折叠
    host.trim().toLowerCase(),
    String(port ?? 22),
    user?.trim() ?? "",
  ].join("::");

/** ssh config 条目是否已被现有 sshHosts 覆盖（决定导入列表里的禁用态） */
export const isSshConfigHostImported = (
  configHost: SshConfigHost,
  existing: SshHostConfig[],
): boolean => {
  const key = sshHostMatchKey(
    configHost.hostName ?? configHost.alias,
    configHost.port,
    configHost.user,
  );
  return existing.some(
    (host) => sshHostMatchKey(host.host, host.port, host.user) === key,
  );
};

/** ssh config 条目 → 待保存的 SshHostConfig（导入路径的唯一构造点） */
export const toSshHostConfig = (configHost: SshConfigHost): SshHostConfig => ({
  id: generateUUID(),
  label: configHost.alias,
  host: configHost.hostName ?? configHost.alias,
  user: configHost.user?.trim() || undefined,
  port: configHost.port,
});

/** 提交前的最后闸口：任一 id 含 "/" 即拒绝（ssh: source_path 往返保护） */
const hasUnsafeHostId = (hosts: SshHostConfig[]): boolean =>
  hosts.some((host) => host.id.includes("/"));

// ─── 行内展示 ───────────────────────────────────────────────────────────────

/** 副文案：user@host:port；user 缺省只写 host，port 缺省不拼 */
const formatHostLine = (host: SshHostConfig): string =>
  `${host.user ? `${host.user}@` : ""}${host.host}${host.port ? `:${host.port}` : ""}`;

type HostTestState =
  | { status: "testing" }
  | { status: "ok" }
  | { status: "fail"; message: string };

/** 测试连接的行内结果：spinner → ✓ 连接成功 / ✗ 连接失败（悬停看后端文案） */
function HostTestResult({ state }: { state: HostTestState }) {
  const { t } = useTranslation();
  if (state.status === "testing") {
    return (
      <Loader2
        className="h-3.5 w-3.5 animate-spin text-fg-3"
        aria-label={t("settings.sshHosts.testing", { defaultValue: "测试中" })}
      />
    );
  }
  if (state.status === "ok") {
    return (
      <span className="inline-flex items-center gap-1 text-caption text-success-text">
        <Check className="h-3.5 w-3.5" />
        {t("settings.sshHosts.testSuccess", { defaultValue: "连接成功" })}
      </span>
    );
  }
  return (
    <HoverTip content={state.message}>
      <span className="inline-flex items-center gap-1 text-caption text-danger-text">
        <X className="h-3.5 w-3.5" />
        {t("settings.sshHosts.testFailed", { defaultValue: "连接失败" })}
      </span>
    </HoverTip>
  );
}

// ─── 手动添加 / 编辑表单 ────────────────────────────────────────────────────

function SshHostForm({
  initial,
  submitLabel,
  onSubmit,
  onCancel,
}: {
  initial?: SshHostConfig | null;
  submitLabel: string;
  onSubmit: (host: SshHostConfig) => Promise<void>;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  const form = useForm<SshHostFormValues, unknown, SshHostConfigDraft>({
    resolver: zodResolver(sshHostFormSchema),
    defaultValues: {
      // 新增在这里取 UUID；编辑沿用原 id（仅作透传，不渲染输入框）
      id: initial?.id ?? generateUUID(),
      label: initial?.label ?? "",
      host: initial?.host ?? "",
      user: initial?.user ?? "",
      port: initial?.port?.toString() ?? "",
      identityFile: initial?.identityFile ?? "",
    },
  });

  const handleSubmit = form.handleSubmit(async (values) => {
    const host: SshHostConfig = {
      id: values.id,
      label: values.label,
      host: values.host,
      user: values.user?.trim() || undefined,
      port: values.port,
      identityFile: values.identityFile?.trim() || undefined,
    };
    await onSubmit(host);
  });

  return (
    <Form {...form}>
      <form onSubmit={handleSubmit} className="flex flex-col gap-4">
        <FormField
          control={form.control}
          name="label"
          render={({ field }) => (
            <FormItem>
              <FormLabel>
                {t("settings.sshHosts.form.label", { defaultValue: "名称" })}
              </FormLabel>
              <FormControl>
                <Input
                  {...field}
                  placeholder={t("settings.sshHosts.form.labelPlaceholder", {
                    defaultValue: "如：build-box",
                  })}
                />
              </FormControl>
              <FormMessage />
            </FormItem>
          )}
        />
        <FormField
          control={form.control}
          name="host"
          render={({ field }) => (
            <FormItem>
              <FormLabel>
                {t("settings.sshHosts.form.host", {
                  defaultValue: "主机地址",
                })}
              </FormLabel>
              <FormControl>
                <Input
                  {...field}
                  placeholder={t("settings.sshHosts.form.hostPlaceholder", {
                    defaultValue: "IP 或域名",
                  })}
                />
              </FormControl>
              <FormMessage />
            </FormItem>
          )}
        />
        {/* user/port 双列：端口窄列先例（CodexFormFields.tsx:1735），user 弹性占满 */}
        <div className="grid grid-cols-[1fr_140px] gap-4">
          <FormField
            control={form.control}
            name="user"
            render={({ field }) => (
              <FormItem>
                <FormLabel>
                  {t("settings.sshHosts.form.user", {
                    defaultValue: "用户名（可选）",
                  })}
                </FormLabel>
                <FormControl>
                  <Input {...field} autoComplete="off" />
                </FormControl>
                <FormMessage />
              </FormItem>
            )}
          />
          <FormField
            control={form.control}
            name="port"
            render={({ field }) => (
              <FormItem>
                <FormLabel>
                  {t("settings.sshHosts.form.port", {
                    defaultValue: "端口（可选）",
                  })}
                </FormLabel>
                <FormControl>
                  <Input
                    {...field}
                    value={field.value ?? ""}
                    inputMode="numeric"
                    placeholder="22"
                    autoComplete="off"
                  />
                </FormControl>
                <FormMessage />
              </FormItem>
            )}
          />
        </div>
        <FormField
          control={form.control}
          name="identityFile"
          render={({ field }) => (
            <FormItem>
              <FormLabel>
                {t("settings.sshHosts.form.identityFile", {
                  defaultValue: "私钥文件（可选）",
                })}
              </FormLabel>
              <FormControl>
                <Input
                  {...field}
                  placeholder="~/.ssh/id_ed25519"
                  autoComplete="off"
                />
              </FormControl>
              <FormMessage />
            </FormItem>
          )}
        />
        {/* 迁入 DialogFooter 统一按钮区规格；px-0 抵消外层 body 的 px-6，
            自身只留 pb-5/pt-3 竖向边距（左右对齐完全由包裹层负责） */}
        <DialogFooter className="px-0">
          <Button
            type="button"
            variant="neutral"
            size="regular"
            onClick={onCancel}
          >
            {t("common.cancel", { defaultValue: "取消" })}
          </Button>
          <Button
            type="submit"
            variant="solid"
            size="regular"
            disabled={form.formState.isSubmitting}
          >
            {form.formState.isSubmitting && (
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            )}
            {submitLabel}
          </Button>
        </DialogFooter>
      </form>
    </Form>
  );
}

// ─── 添加对话框（从 SSH Config 导入 / 手动添加 双 tab） ─────────────────────

function AddHostDialog({
  open,
  existingHosts,
  onClose,
  onAdd,
}: {
  open: boolean;
  existingHosts: SshHostConfig[];
  onClose: () => void;
  /** 返回是否保存成功；成功后由调用方关对话框 */
  onAdd: (hosts: SshHostConfig[]) => Promise<boolean>;
}) {
  const { t } = useTranslation();
  const [selected, setSelected] = useState<Set<string>>(new Set());

  const configQuery = useQuery({
    queryKey: ["sshConfigHosts"],
    queryFn: () => sessionsApi.listSshConfigHosts(),
    enabled: open,
  });

  useEffect(() => {
    // 每次打开都从上次的选择重新开始
    if (open) setSelected(new Set());
  }, [open]);

  const toggleAlias = (alias: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(alias)) {
        next.delete(alias);
      } else {
        next.add(alias);
      }
      return next;
    });
  };

  const handleImport = async () => {
    const picked = (configQuery.data ?? []).filter((configHost) =>
      selected.has(configHost.alias),
    );
    if (picked.length === 0) return;
    const added = await onAdd(picked.map(toSshHostConfig));
    if (added) onClose();
  };

  return (
    <Dialog open={open} onOpenChange={(next) => !next && onClose()}>
      <DialogContent zIndex="nested" className="max-w-md">
        <DialogHeader>
          <DialogTitle>
            {t("settings.sshHosts.addTitle", { defaultValue: "添加 SSH 主机" })}
          </DialogTitle>
          <DialogDescription className="sr-only">
            {t("settings.sshHosts.addDescription", {
              defaultValue:
                "从 ~/.ssh/config 导入已有主机，或手动填写一台新主机。",
            })}
          </DialogDescription>
        </DialogHeader>
        {/* body 包裹层：DialogContent 本体无 padding、Header/Footer 自带
            px-6——中间内容靠这层与标题左右对齐（NewLayoutDialog.tsx:122
            惯例）。不加 pb：底距交给各 tab 内 DialogFooter 的 pb-5，防叠加 */}
        <div className="px-6">
          <Tabs defaultValue="import">
            <TabsList className="w-full">
              <TabsTrigger value="import" className="flex-1">
                {t("settings.sshHosts.importTab", {
                  defaultValue: "从 SSH Config 导入",
                })}
              </TabsTrigger>
              <TabsTrigger value="manual" className="flex-1">
                {t("settings.sshHosts.manualTab", { defaultValue: "手动添加" })}
              </TabsTrigger>
            </TabsList>
            <TabsContent value="import" className="mt-4">
              {configQuery.isPending ? (
                <div className="flex items-center justify-center gap-2 py-8 text-body text-fg-2">
                  <Loader2 className="h-4 w-4 animate-spin" />
                  {t("settings.sshHosts.import.loading", {
                    defaultValue: "正在读取 ~/.ssh/config…",
                  })}
                </div>
              ) : configQuery.isError ? (
                <div className="py-6 text-center text-body text-fg-2">
                  {t("settings.sshHosts.import.error", {
                    defaultValue: "读取 SSH Config 失败：{{error}}",
                    error: String(
                      (configQuery.error as Error)?.message ??
                        configQuery.error,
                    ),
                  })}
                </div>
              ) : (configQuery.data ?? []).length === 0 ? (
                <div className="py-6 text-center text-body text-fg-2">
                  {t("settings.sshHosts.import.empty", {
                    defaultValue:
                      "~/.ssh/config 里没有可导入的 Host 段，可切到「手动添加」。",
                  })}
                </div>
              ) : (
                <>
                  <ul className="m-0 max-h-72 list-none overflow-y-auto rounded-panel border border-border bg-subtle p-1.5">
                    {(configQuery.data ?? []).map((configHost) => {
                      const imported = isSshConfigHostImported(
                        configHost,
                        existingHosts,
                      );
                      return (
                        <li key={configHost.alias}>
                          <label
                            className={`flex cursor-pointer items-center gap-3 rounded-control px-2.5 py-1.5 text-body hover:bg-surface ${
                              imported ? "pointer-events-none opacity-50" : ""
                            }`}
                          >
                            <Checkbox
                              checked={
                                imported || selected.has(configHost.alias)
                              }
                              disabled={imported}
                              onCheckedChange={() =>
                                toggleAlias(configHost.alias)
                              }
                              aria-label={configHost.alias}
                            />
                            <span className="min-w-0 flex-1">
                              <span className="block truncate font-medium text-fg-1">
                                {configHost.alias}
                              </span>
                              <span className="block truncate text-caption text-fg-2">
                                {formatHostLine({
                                  id: "",
                                  label: "",
                                  host: configHost.hostName ?? configHost.alias,
                                  user: configHost.user,
                                  port: configHost.port,
                                })}
                              </span>
                            </span>
                            {imported && (
                              <span className="shrink-0 text-caption text-fg-3">
                                {t("settings.sshHosts.import.imported", {
                                  defaultValue: "已导入",
                                })}
                              </span>
                            )}
                          </label>
                        </li>
                      );
                    })}
                  </ul>
                  {/* px-0 抵消外层 body 的 px-6，与手动 tab 的 Footer 同构 */}
                  <DialogFooter className="px-0">
                    <Button
                      variant="solid"
                      size="regular"
                      disabled={selected.size === 0}
                      onClick={() => void handleImport()}
                    >
                      {t("settings.sshHosts.import.action", {
                        defaultValue: "导入所选 {{count}} 台主机",
                        count: selected.size,
                      })}
                    </Button>
                  </DialogFooter>
                </>
              )}
            </TabsContent>
            <TabsContent value="manual" className="mt-4">
              <SshHostForm
                initial={null}
                submitLabel={t("settings.sshHosts.form.submitAdd", {
                  defaultValue: "添加",
                })}
                onSubmit={async (host) => {
                  const added = await onAdd([host]);
                  if (added) onClose();
                }}
                onCancel={onClose}
              />
            </TabsContent>
          </Tabs>
        </div>
      </DialogContent>
    </Dialog>
  );
}

// ─── 编辑对话框（复用手动表单，仅改标题与提交语义） ─────────────────────────

function EditHostDialog({
  host,
  onClose,
  onEdit,
}: {
  host: SshHostConfig | null;
  onClose: () => void;
  onEdit: (host: SshHostConfig) => Promise<boolean>;
}) {
  const { t } = useTranslation();
  return (
    <Dialog open={!!host} onOpenChange={(next) => !next && onClose()}>
      <DialogContent zIndex="nested" className="max-w-md">
        <DialogHeader>
          <DialogTitle>
            {t("settings.sshHosts.editTitle", {
              defaultValue: "编辑主机「{{label}}」",
              label: host?.label ?? "",
            })}
          </DialogTitle>
          <DialogDescription className="sr-only">
            {t("settings.sshHosts.editDescription", {
              defaultValue: "修改该主机的名称、地址与连接参数。",
            })}
          </DialogDescription>
        </DialogHeader>
        {/* 表单区包 px-6 与 Header 对齐；底距交给表单内 DialogFooter 的 pb-5 */}
        {host && (
          <div className="px-6">
            <SshHostForm
              initial={host}
              submitLabel={t("common.save", { defaultValue: "保存" })}
              onSubmit={async (next) => {
                const saved = await onEdit(next);
                if (saved) onClose();
              }}
              onCancel={onClose}
            />
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

// ─── 分区主体 ───────────────────────────────────────────────────────────────

interface SshHostsSectionProps {
  settings: SettingsFormState;
  onAutoSave: (updates: Partial<SettingsFormState>) => Promise<boolean>;
}

export function SshHostsSection({
  settings,
  onAutoSave,
}: SshHostsSectionProps) {
  const { t } = useTranslation();
  const hosts = settings.sshHosts ?? [];

  const [addOpen, setAddOpen] = useState(false);
  const [editingHost, setEditingHost] = useState<SshHostConfig | null>(null);
  const [deletingHost, setDeletingHost] = useState<SshHostConfig | null>(null);
  const [testStates, setTestStates] = useState<
    Record<string, HostTestState | undefined>
  >({});

  // 全量提交新列表；id 含 "/" 的坏数据在 schema 之外再拦一道
  const commitHosts = async (next: SshHostConfig[]): Promise<boolean> => {
    if (hasUnsafeHostId(next)) {
      toast.error(
        t("settings.sshHosts.badId", {
          defaultValue: "主机 ID 含非法字符 “/”，已取消保存",
        }),
      );
      return false;
    }
    return onAutoSave({ sshHosts: next });
  };

  const handleTest = async (host: SshHostConfig) => {
    setTestStates((prev) => ({ ...prev, [host.id]: { status: "testing" } }));
    try {
      await sessionsApi.testSshHost(host);
      setTestStates((prev) => ({ ...prev, [host.id]: { status: "ok" } }));
    } catch (error) {
      setTestStates((prev) => ({
        ...prev,
        [host.id]: {
          status: "fail",
          // 后端 test_ssh_host 抛的中文分类文案（连接 / 认证 / 超时 / ssh 缺失）
          message:
            (error as Error)?.message?.trim() || String(error ?? "unknown"),
        },
      }));
    }
  };

  const handleAdd = async (added: SshHostConfig[]): Promise<boolean> => {
    const saved = await commitHosts([...hosts, ...added]);
    if (saved) {
      toast.success(
        t("settings.sshHosts.addedToast", {
          defaultValue: "已添加 {{count}} 台主机",
          count: added.length,
        }),
      );
    }
    return saved;
  };

  const handleEdit = async (next: SshHostConfig): Promise<boolean> =>
    commitHosts(hosts.map((host) => (host.id === next.id ? next : host)));

  const confirmDelete = async () => {
    if (!deletingHost) return;
    const removed = deletingHost;
    setDeletingHost(null);
    // 顺序锁定：先 purge 再 save_settings——两命令都按 settings 校验主机
    // 存在，save 先行会让 purge 拒绝执行（主机已不在清单），本地数据残留
    try {
      await usageApi.purgeRemoteHostData(removed.id);
    } catch (error) {
      // purge 是尽力清理：用户已确认删除意图，失败仅警告、不阻断删除主机
      toast.warning(
        t("settings.sshHosts.purgeFailed", {
          defaultValue: "数据清理未完全成功：{{error}}",
          error: String(error),
        }),
      );
    }
    const saved = await commitHosts(
      hosts.filter((host) => host.id !== removed.id),
    );
    if (saved) {
      toast.success(
        t("settings.sshHosts.deletedToast", {
          defaultValue: "已删除主机「{{label}}」",
          label: removed.label,
        }),
      );
    }
  };

  return (
    <SettingsBlock
      title={t("settings.sshHosts.blockTitle", {
        defaultValue: "SSH 远程主机",
      })}
      help={{
        title: t("settings.sshHosts.blockTitle", {
          defaultValue: "SSH 远程主机",
        }),
        body: t("settings.sshHosts.help", {
          defaultValue:
            "配置会话页主机切换可用的远程机器。可从 ~/.ssh/config 导入或手动添加；删除主机会一并清理它的本地用量数据与会话缓存。",
        }),
      }}
      actions={
        <Button
          variant="neutral"
          size="compact"
          onClick={() => setAddOpen(true)}
        >
          <Plus className="h-3.5 w-3.5" />
          {t("settings.sshHosts.add", { defaultValue: "添加主机" })}
        </Button>
      }
    >
      {hosts.length === 0 ? (
        <div className="rounded-panel border border-border bg-surface px-4 py-8 text-center text-body text-fg-2">
          {t("settings.sshHosts.empty", {
            defaultValue:
              "还没有 SSH 主机。添加后即可在会话页切换查看远程会话。",
          })}
        </div>
      ) : (
        <SettingsCard>
          {hosts.map((host) => {
            const testState = testStates[host.id];
            return (
              <SettingsRow
                key={host.id}
                label={host.label}
                description={formatHostLine(host)}
                control={
                  <>
                    {testState && <HostTestResult state={testState} />}
                    <Button
                      variant="neutral"
                      size="compact"
                      disabled={testState?.status === "testing"}
                      onClick={() => void handleTest(host)}
                    >
                      <PlugZap className="h-3.5 w-3.5" />
                      {t("settings.sshHosts.testConnection", {
                        defaultValue: "测试连接",
                      })}
                    </Button>
                    <HoverTip
                      content={t("settings.sshHosts.edit", {
                        defaultValue: "编辑",
                      })}
                    >
                      <Button
                        variant="ghost"
                        size="icon-compact"
                        aria-label={t("settings.sshHosts.edit", {
                          defaultValue: "编辑",
                        })}
                        onClick={() => setEditingHost(host)}
                      >
                        <Pencil className="h-3.5 w-3.5" />
                      </Button>
                    </HoverTip>
                    <HoverTip
                      content={t("settings.sshHosts.delete", {
                        defaultValue: "删除",
                      })}
                    >
                      <Button
                        variant="ghost"
                        size="icon-compact"
                        aria-label={t("settings.sshHosts.delete", {
                          defaultValue: "删除",
                        })}
                        onClick={() => setDeletingHost(host)}
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </Button>
                    </HoverTip>
                  </>
                }
              />
            );
          })}
        </SettingsCard>
      )}

      <AddHostDialog
        open={addOpen}
        existingHosts={hosts}
        onClose={() => setAddOpen(false)}
        onAdd={handleAdd}
      />

      <EditHostDialog
        host={editingHost}
        onClose={() => setEditingHost(null)}
        onEdit={handleEdit}
      />

      {/* 删除确认：连带清理是破坏性副作用（本地用量数据 + 会话缓存），
          说明清楚再动手；确认后走 purge → save_settings（见 confirmDelete） */}
      <Dialog
        open={!!deletingHost}
        onOpenChange={(next) => !next && setDeletingHost(null)}
      >
        <DialogContent role="alertdialog" zIndex="alert" className="max-w-md">
          <DialogHeader>
            <DialogTitle>
              {t("settings.sshHosts.deleteTitle", {
                defaultValue: "删除主机「{{label}}」？",
                label: deletingHost?.label ?? "",
              })}
            </DialogTitle>
            <DialogDescription>
              {t("settings.sshHosts.deleteLead", {
                defaultValue: "该主机将从列表移除，连接配置一并清除。",
              })}
              <br />
              {t("settings.sshHosts.deletePurgeNote", {
                defaultValue:
                  "将同时清理该主机的本地用量数据与会话缓存；远端文件不受影响，重新添加后可重新同步。",
              })}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="neutral"
              size="regular"
              autoFocus
              onClick={() => setDeletingHost(null)}
            >
              {t("common.cancel", { defaultValue: "取消" })}
            </Button>
            <Button
              variant="destructive"
              size="regular"
              onClick={() => void confirmDelete()}
            >
              {t("settings.sshHosts.deleteConfirm", {
                defaultValue: "删除主机",
              })}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </SettingsBlock>
  );
}
