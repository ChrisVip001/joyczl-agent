import { el } from "./render";
import { t } from "./i18n";
import { reconcileTaskOperation, taskOperations, reconcileTurnOperation, turnOperations } from "./api";
import type { AgentTaskView, TaskOperationsResponse, ToolOperationView } from "./protocol";

/** Human observations settle uncertainty; this form never resumes or executes work. */
export function taskOperationForm(
  task: AgentTaskView, onSaved: () => void, onClose: () => void,
  onDetachedError: (error: unknown) => void,
): HTMLFormElement {
  return operationForm({ id: task.id, title: task.title, load: () => taskOperations(task.id),
    reconcile: (operation, digest, resolution, evidence) => reconcileTaskOperation({ taskId: task.id,
      operationId: operation.id, checkpointDigest: digest, resolution, evidence }),
  }, onSaved, onClose, onDetachedError);
}

export function chatOperationForm(
  sessionId: string, onSaved: () => void, onClose: () => void,
  onDetachedError: (error: unknown) => void,
): HTMLFormElement {
  return operationForm({ id: `chat-${encodeURIComponent(sessionId)}`, title: t("当前会话的操作记录"), load: () => turnOperations({sessionId}),
    reconcile: (operation, digest, resolution, evidence) => reconcileTurnOperation({ sessionId,
      turnId: operation.turnId, operationId: operation.id, checkpointDigest: digest, resolution, evidence }),
  }, onSaved, onClose, onDetachedError);
}

function operationForm(
  target: { id: string; title: string; load: () => Promise<TaskOperationsResponse>;
    reconcile: (operation: ToolOperationView, digest: string, resolution: string, evidence: string) => Promise<TaskOperationsResponse> },
  onSaved: () => void, onClose: () => void, onDetachedError: (error: unknown) => void,
): HTMLFormElement {
  const id = `operations-${target.id}`;
  const panel = el("form", { class: "fspanel cap-form operation-dialog", role: "dialog", "aria-modal": "true", "aria-labelledby": `${id}-heading` });
  const selection = el("select", { id: `${id}-selection`, required: true });
  const origin = el("p", { class: "hint" });
  const argumentsView = el("pre", { class: "task-input-data", translate: "no" });
  const resolution = el("select", { id: `${id}-resolution`, required: true },
    el("option", { value: "", text: t("选择核对结果") }),
    el("option", { value: "reconciled_success", text: t("已确认外部效果成功") }),
    el("option", { value: "reconciled_failure", text: t("已确认外部操作失败") }),
    el("option", { value: "not_executed", text: t("已确认未执行") }),
  );
  const evidence = el("textarea", { id: `${id}-evidence`, class: "cap-textarea", rows: 5, required: true, maxlength: 12000 });
  const error = el("p", { class: "failure", role: "alert", hidden: true });
  const status = el("p", { class: "hint", role: "status", "aria-live": "polite", text: t("正在加载操作…") });
  const save = el("button", { class: "approve", type: "submit", disabled: true, text: t("记录核对结果") });
  const reload = el("button", { class: "ghost", type: "button", text: t("重新读取操作") });
  const close = el("button", { class: "ghost", type: "button", text: t("关闭"), onclick: onClose });
  const fields = el("div", { class: "operation-fields", hidden: true },
    el("label", { for: selection.id, text: t("待核对操作") }), selection, origin, argumentsView,
    el("label", { for: resolution.id, text: t("核对结果") }), resolution,
    el("label", { for: evidence.id, text: t("核对证据或外部回执") }), evidence,
  );
  const receipts = el("div");
  const body = el("div", { class: "operation-body" }, el("h2", { id: `${id}-heading`, text: t("核对操作") }),
    el("p", { class: "hint", text: target.title }),
    el("p", { class: "hint", text: t("先检查真实外部状态。记录结果不会执行或继续任务，也不代替原验收。") }),
    status, error, fields, receipts);
  panel.append(body, el("div", { class: "row operation-actions" }, save, reload, close));
  let snapshot: TaskOperationsResponse | undefined;
  let pending = false;
  let lastSelected = "";
  function freezeFields(value: boolean): void {
    selection.disabled = value; resolution.disabled = value; evidence.disabled = value;
    if (value) close.focus();
    else if (panel.isConnected && (document.activeElement === close || document.activeElement === document.body)) {
      (fields.hidden ? close : error.hidden ? selection : evidence).focus();
    }
  }
  function selectedDigest(): string | undefined {
    return snapshot?.operations.find(operation => operation.id === selection.value)?.checkpointDigest ?? snapshot?.checkpointDigest ?? undefined;
  }
  function paint(next: TaskOperationsResponse): void {
    const previous = selection.value;
    snapshot = next;
    const unknown = next.operations.filter(operation => operation.status === "intent");
    selection.replaceChildren(...unknown.map(operation => el("option", { value: operation.id, text: `${operation.tool} · ${operation.toolUseId}` })));
    if (unknown.some(operation => operation.id === previous)) selection.value = previous;
    fields.hidden = unknown.length === 0;
    save.disabled = unknown.length === 0 || !selectedDigest();
    status.textContent = unknown.length ? t("尚有 {count} 个操作需要核对", { count: unknown.length }) : t("没有待核对操作；继续仍需明确授权。");
    receipts.replaceChildren(...next.operations.filter(operation => operation.evidence).map(operation =>
      el("details", {}, el("summary", { text: `${operation.tool} · ${operation.status}` }),
        el("pre", { class: "task-input-data", translate: "no", text: operation.evidence ?? "" }))));
    selection.dispatchEvent(new Event("change"));
  }
  selection.addEventListener("change", () => {
    if (selection.value !== lastSelected) { resolution.value = ""; evidence.value = ""; }
    lastSelected = selection.value;
    save.disabled = fields.hidden || !selectedDigest();
    const operation = snapshot?.operations.find(row => row.id === selection.value);
    origin.textContent = operation?.origin === "recovered_checkpoint"
      ? t("由旧检查点重建；无法确认是否已发送。")
      : t("执行前已保存意图；这不证明外部操作已完成。");
    argumentsView.textContent = operation ? JSON.stringify(operation.arguments, null, 2) : "";
  });
  function showError(failure: unknown): void {
    if (!panel.isConnected) { onDetachedError(failure); return; }
    status.textContent = "";
    error.hidden = false;
    error.textContent = failure instanceof Error ? failure.message : String(failure);
  }
  panel.addEventListener("keydown", event => {
    if (event.key === "Escape") { event.stopPropagation(); onClose(); }
    if (event.key !== "Tab") return;
    const controls = [...panel.querySelectorAll<HTMLElement>("select:not(:disabled),textarea:not(:disabled),button:not(:disabled),summary")].filter(control => control.offsetParent !== null);
    const first = controls[0]; const last = controls.at(-1);
    if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
    else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
  });
  reload.addEventListener("click", () => {
    if (pending) return;
    const selected = selection.value;
    pending = true; freezeFields(true); reload.disabled = true; save.disabled = true; error.hidden = true;
    status.textContent = t("正在加载操作…");
    void target.load().then(next => {
      if (!panel.isConnected) return;
      paint(next);
      if (next.operations.some(operation => operation.id === selected && operation.status === "intent")) {
        selection.value = selected; selection.dispatchEvent(new Event("change"));
      } else { resolution.value = ""; evidence.value = ""; }
    }).catch(showError).finally(() => {
      pending = false; freezeFields(false); reload.disabled = false; save.disabled = fields.hidden || !selectedDigest();
    });
  });
  panel.addEventListener("submit", event => {
    event.preventDefault();
    const operation = snapshot?.operations.find(operation => operation.id === selection.value);
    const digest = selectedDigest();
    if (pending || !operation || !digest || !panel.reportValidity()) return;
    if (!evidence.value.trim() || new TextEncoder().encode(evidence.value).byteLength > 16384) {
      showError(new Error("Provide 1 to 16384 bytes of reconciliation evidence")); evidence.focus(); return;
    }
    pending = true; freezeFields(true); save.disabled = true; reload.disabled = true; error.hidden = true;
    void target.reconcile(operation, digest, resolution.value, evidence.value)
      .then(next => {
        if (!panel.isConnected) { onSaved(); return; }
        paint(next); resolution.value = ""; evidence.value = ""; onSaved();
        (fields.hidden ? close : selection).focus();
      }).catch(showError).finally(() => { pending = false; freezeFields(false); reload.disabled = false; save.disabled = fields.hidden || !selectedDigest(); });
  });
  void target.load().then(next => { if (panel.isConnected) paint(next); }).catch(showError);
  return panel;
}
