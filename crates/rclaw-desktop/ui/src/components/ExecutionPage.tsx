/** 执行列表 + 详情钻取容器（Agent 执行 / 任务概览共用）。
 *
 * 详情打开时列表保留并 `visibility:hidden`（保住滚动位置），返回时把焦点还给原卡片，
 * 卡片已被清理则退到第一张。
 */
import { useRef, type ReactNode } from "react";

import AgentExecutionDetail from "./AgentExecutionDetail";

export default function ExecutionPage({
  selectedAgentId,
  onBack,
  children,
}: {
  selectedAgentId: string | null;
  onBack: () => void;
  children: (hidden: boolean) => ReactNode;
}) {
  const listRef = useRef<HTMLDivElement | null>(null);
  const back = () => {
    const agentId = selectedAgentId;
    onBack();
    if (agentId === null) return;
    requestAnimationFrame(() => {
      const list = listRef.current;
      if (!list) return;
      const card =
        list.querySelector<HTMLElement>(`[data-agent-id="${CSS.escape(agentId)}"]`) ??
        list.querySelector<HTMLElement>("[data-agent-id]");
      card?.focus({ preventScroll: card.dataset.agentId === agentId });
    });
  };
  const showingDetail = selectedAgentId !== null;
  return (
    <div className="relative h-full">
      <div
        ref={listRef}
        className={showingDetail ? "invisible absolute bottom-0 left-0 right-0 top-0 overflow-hidden" : "h-full"}
        aria-hidden={showingDetail}
      >
        {children(showingDetail)}
      </div>
      {selectedAgentId !== null && (
        <AgentExecutionDetail key={selectedAgentId} agentId={selectedAgentId} onBack={back} />
      )}
    </div>
  );
}
