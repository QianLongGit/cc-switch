import { Channel, invoke } from "@tauri-apps/api/core";
import type {
  ContentRef,
  ImageRef,
  SessionContentSearchResult,
  SessionIndexStatus,
  SessionMessage,
  SessionMeta,
  SshConfigHost,
  SshHostConfig,
  TranscriptChunk,
} from "@/types";

export interface DeleteSessionOptions {
  providerId: string;
  sessionId: string;
  sourcePath: string;
}

export interface DeleteSessionResult extends DeleteSessionOptions {
  success: boolean;
  error?: string;
}

/** get_session_block_content 的返回：按字符分页取工具输出 / 参数 / 思考 / diff 全文 */
export interface BlockContent {
  text: string;
  /** 全文字符数 */
  totalLen: number;
  /** 本页之后还有内容 */
  truncated: boolean;
  /** 下一页的起始字符偏移 */
  nextOffset?: number;
}

export interface BlockContentPage {
  offset?: number;
  limit?: number;
}

export const sessionsApi = {
  /** hostId 省略时列本机会话；传主机 id 时列该 SSH 主机上的会话 */
  async list(hostId?: string): Promise<SessionMeta[]> {
    return await invoke("list_sessions", { hostId: hostId ?? null });
  },

  /** 解析 ~/.ssh/config 的 Host 段（主机选择下拉的数据源） */
  async listSshConfigHosts(): Promise<SshConfigHost[]> {
    return await invoke("list_ssh_config_hosts");
  },

  /** 测试单台主机连通性；失败文案来自后端，直接给 UI 展示 */
  async testSshHost(host: SshHostConfig): Promise<boolean> {
    return await invoke("test_ssh_host", { host });
  },

  /** 本机 ssh 二进制是否可用（远程功能入口的可用性开关） */
  async checkSshAvailability(): Promise<boolean> {
    return await invoke("check_ssh_availability");
  },

  /** 后台同步正文索引；立即返回当前进度，之后的进度走 session-index-status 事件 */
  async syncContentIndex(): Promise<SessionIndexStatus> {
    return await invoke("sync_session_index");
  },

  /** 在消息正文里搜索（按会话聚合）；providerIds 为空时不按应用过滤 */
  async searchContent(
    query: string,
    providerIds?: string[],
  ): Promise<SessionContentSearchResult> {
    return await invoke("search_session_content", { query, providerIds });
  },

  /** 删除正文索引文件（关闭「搜索消息正文」时） */
  async clearContentIndex(): Promise<boolean> {
    return await invoke("clear_session_index");
  },

  async getMessages(
    providerId: string,
    sourcePath: string,
  ): Promise<SessionMessage[]> {
    return await invoke("get_session_messages", { providerId, sourcePath });
  },

  /**
   * 分块读取会话（stream_session_messages）：后端先推 Header，再按批推 Messages，
   * 最后 Done；失败时先推 Error 再以同一文案 reject。
   */
  async streamMessages(
    providerId: string,
    sourcePath: string,
    onChunk: (chunk: TranscriptChunk) => void,
  ): Promise<void> {
    const channel = new Channel<TranscriptChunk>();
    channel.onmessage = onChunk;
    await invoke("stream_session_messages", {
      providerId,
      sourcePath,
      onChunk: channel,
    });
  },

  /** 按 ContentRef 取全文（引用原样回传，前端不解析）；按字符分页，每页最多 512K 字符 */
  async getBlockContent(
    providerId: string,
    sourcePath: string,
    contentRef: ContentRef,
    page: BlockContentPage = {},
  ): Promise<BlockContent> {
    return await invoke("get_session_block_content", {
      providerId,
      sourcePath,
      contentRef,
      offset: page.offset,
      limit: page.limit,
    });
  },

  /** 取图片原始字节（后端返回 tauri::ipc::Response），调用方按 mediaType 转 Blob；SVG 会被后端拒绝 */
  async getImage(
    providerId: string,
    sourcePath: string,
    image: ImageRef,
  ): Promise<ArrayBuffer> {
    return await invoke("get_session_image", {
      providerId,
      sourcePath,
      image,
    });
  },

  /** 在 Finder / 文件管理器中显示本地路径（绝对路径或 file://，决策 D3） */
  async revealPath(path: string): Promise<boolean> {
    return await invoke("reveal_session_path", { path });
  },

  /** 弹出保存对话框把会话存成 Markdown 文件；取消时返回 null，成功返回保存路径 */
  async exportMarkdown(
    defaultName: string,
    content: string,
  ): Promise<string | null> {
    return await invoke("export_session_markdown", { defaultName, content });
  },

  async delete(options: DeleteSessionOptions): Promise<boolean> {
    const { providerId, sessionId, sourcePath } = options;
    return await invoke("delete_session", {
      providerId,
      sessionId,
      sourcePath,
    });
  },

  async deleteMany(
    items: DeleteSessionOptions[],
  ): Promise<DeleteSessionResult[]> {
    return await invoke("delete_sessions", { items });
  },

  async launchTerminal(options: {
    command: string;
    cwd?: string | null;
    customConfig?: string | null;
  }): Promise<boolean> {
    const { command, cwd, customConfig } = options;
    return await invoke("launch_session_terminal", {
      command,
      cwd,
      customConfig,
    });
  },
};
