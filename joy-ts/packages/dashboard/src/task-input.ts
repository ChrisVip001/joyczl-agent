import { el } from "./render";
import { t } from "./i18n";
import { signalTaskInput, taskAction } from "./api";
import type { AgentTaskView } from "./protocol";

/** Human input controls preserve the form on refusal and never start a task. */
export function taskInputForm(
  task: AgentTaskView,
  mode: "wait" | "input",
  onSaved: () => void,
  onClose: () => void,
  onDetachedError: (error: unknown) => void,
): HTMLFormElement {
  const id = `input-${task.id}`;
  const panel = el("form", {
    class: "fspanel cap-form", role: "dialog", "aria-modal": "true",
    "aria-labelledby": `${id}-heading`,
  });
  const body = el("textarea", {
    id: `${id}-body`, class: "cap-textarea", rows: 6, required: true,
    maxlength: mode === "wait" ? 4096 : 12000,
    "aria-describedby": `${id}-help ${id}-error`,
  });
  const deadline = el("input", { id: `${id}-deadline`, type: "datetime-local" });
  const error = el("p", { id: `${id}-error`, class: "failure", role: "alert", hidden: true });
  const status = el("p", { class: "hint", role: "status", "aria-live": "polite" });
  const save = el("button", { class: "approve", type: "submit", text: mode === "wait" ? t("保存等待条件") : t("提交资料") });
  const close = el("button", { class: "ghost", type: "button", text: t("取消"), onclick: onClose });
  const eventId = Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, "0")).join("");
  let pending = false;
  panel.append(
    el("h2", { id: `${id}-heading`, text: mode === "wait" ? t("等待资料") : t("提供资料") }),
    el("p", { class: "hint", text: task.title }),
    el("label", { for: body.id, text: mode === "wait" ? t("需要哪些资料？") : task.inputCondition?.description ?? t("资料内容") }),
    body,
    el("p", { id: `${id}-help`, class: "hint", text: mode === "wait"
      ? t("先保存等待条件，再开始任务。等待期间可以继续聊天。")
      : t("资料到达后会继续检查任务；收到资料不代表验收通过。") }),
  );
  if (mode === "wait") panel.append(el("label", { for: deadline.id, text: t("资料最晚到达时间（可选）") }), deadline);
  panel.append(error, status, el("div", { class: "row" }, save, close));
  panel.addEventListener("keydown", (event) => {
    if (event.key === "Escape") { event.stopPropagation(); onClose(); }
    if (event.key !== "Tab") return;
    const controls = [...panel.querySelectorAll<HTMLElement>("textarea, input, button:not(:disabled)")];
    const first = controls[0];
    const last = controls.at(-1);
    if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
    else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
  });
  panel.addEventListener("submit", (event) => {
    event.preventDefault();
    if (pending || !panel.reportValidity()) return;
    error.hidden = true;
    const encoded = mode === "wait" ? body.value.trim() : JSON.stringify(body.value);
    if (!body.value.trim() || new TextEncoder().encode(encoded).byteLength > (mode === "wait" ? 4096 : 16384)) {
      error.textContent = body.value.trim() ? "Input is too long. Provide a shorter summary or a file reference." : "Provide the requested input";
      error.hidden = false;
      body.focus();
      return;
    }
    pending = true;
    save.disabled = true;
    status.textContent = t("正在保存…");
    void (async () => {
      try {
        if (mode === "wait") {
          await taskAction({ action: "wait", taskId: task.id, source: "manual", eventKey: task.id,
            description: body.value.trim(), expiresAt: deadline.value ? new Date(deadline.value).toISOString() : null });
        } else {
          const condition = task.inputCondition;
          if (!condition || condition.source !== "manual") throw new Error("This input requires its declared source");
          if (!body.value.trim()) throw new Error("Provide the requested input");
          await signalTaskInput({ conditionId: condition.conditionId, source: condition.source,
            eventKey: condition.eventKey, eventId, data: body.value });
        }
        onSaved();
      } catch (failure) {
        if (!panel.isConnected) { onDetachedError(failure); return; }
        error.textContent = failure instanceof Error ? failure.message : String(failure);
        error.hidden = false;
        body.focus();
      } finally {
        pending = false;
        save.disabled = false;
        status.textContent = "";
      }
    })();
  });
  return panel;
}
