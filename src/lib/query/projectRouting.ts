import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";
import { projectRoutingApi } from "@/lib/api/projectRouting";

export const projectRoutingKeys = {
  projects: ["projectRoutingProjects"] as const,
};

// ========== 项目路由 Hooks ==========

/**
 * 获取项目路由列表（页面激活时 3s 轮询，后台挂起）
 */
export function useProjectRoutingQuery(enabled: boolean) {
  return useQuery({
    queryKey: projectRoutingKeys.projects,
    queryFn: () => projectRoutingApi.listProjects(),
    // 仅页面激活时轮询（仿 useProxyStatusQuery 的条件轮询写法）
    refetchInterval: enabled ? 3000 : false,
    enabled,
    // 保持之前的数据，避免轮询闪烁
    placeholderData: (previousData) => previousData,
  });
}

/**
 * 绑定项目 → 供应商，成功后刷新列表
 */
export function useSetProjectRoute() {
  const queryClient = useQueryClient();
  const { t } = useTranslation();

  return useMutation({
    mutationFn: ({
      projectPath,
      providerId,
    }: {
      projectPath: string;
      providerId: string;
    }) => projectRoutingApi.setProjectRoute(projectPath, providerId),
    onSuccess: () => {
      toast.success(t("projectRouting.toast.saved"), { closeButton: true });
      queryClient.invalidateQueries({ queryKey: projectRoutingKeys.projects });
    },
    onError: (error: Error) => {
      toast.error(
        t("projectRouting.toast.failed", { error: error.message }),
      );
    },
  });
}

/**
 * 解除项目绑定，成功后刷新列表
 */
export function useClearProjectRoute() {
  const queryClient = useQueryClient();
  const { t } = useTranslation();

  return useMutation({
    mutationFn: (projectPath: string) =>
      projectRoutingApi.clearProjectRoute(projectPath),
    onSuccess: () => {
      toast.success(t("projectRouting.toast.saved"), { closeButton: true });
      queryClient.invalidateQueries({ queryKey: projectRoutingKeys.projects });
    },
    onError: (error: Error) => {
      toast.error(
        t("projectRouting.toast.failed", { error: error.message }),
      );
    },
  });
}
