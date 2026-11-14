// The login card: the one screen a remote Joy asks before showing anything.
//
// Centered, single column, the M1 design language at its quietest — a wordmark, four
// fields, two actions. It replaces the whole view while visible (fixed overlay) and
// removes itself on success. Server, username, password, device — the device name is
// what makes "signing in is pairing" visible: the token this card earns belongs to this device.

import { loginAccount, registerAccount, setServerBase, serverBase } from "./api";
import { t } from "./i18n";
import { failure } from "./render";
import { el } from "./render";

/** Where this login came from — the card shows it as the first line under the brand. */
export interface LoginRequest {
  /** Why the card appeared ("your sign-in expired…"). Absent on first run. */
  notice?: string;
  /** Called once an account exists and the token is stored. */
  onAuthed: () => void;
}

/** A sensible device label: this browser on this machine, trimmed to the server's rule. */
function guessDevice(): string {
  const raw = `Web · ${navigator.platform || "browser"}`;
  return raw.length > 32 ? raw.slice(0, 32) : raw;
}

export function showLogin(request: LoginRequest): void {
  const view = el("div", { class: "login-view", role: "dialog", "aria-label": t("登录 Joy") });
  const card = el("form", { class: "login-card" });
  card.noValidate = true;

  const errorSlot = el("div", { class: "login-error" });

  const server = el("input", {
    type: "url",
    placeholder: t("服务器地址（默认本机）"),
    value: serverBase(),
    autocomplete: "url",
    spellcheck: "false",
  });
  const username = el("input", {
    type: "text",
    placeholder: t("用户名（小写字母数字连字符）"),
    autocomplete: "username",
    autocapitalize: "none",
  });
  const password = el("input", {
    type: "password",
    placeholder: t("密码（至少 8 个字符）"),
    autocomplete: "current-password",
  });
  const device = el("input", { type: "text", value: guessDevice(), "aria-label": t("设备名") });

  card.append(
    el(
      "div",
      { class: "login-brand" },
      el("span", { class: "dot", "aria-hidden": "true" }),
      el("span", { text: "Joy" }),
    ),
    request.notice === undefined
      ? el("p", { class: "login-hint", text: t("连接你的 Joy 服务器。首次使用会注册一个新账号。") })
      : el("p", { class: "login-hint", text: request.notice }),
    server,
    username,
    password,
    device,
    errorSlot,
    el(
      "div",
      { class: "login-actions" },
      el("button", { class: "login-primary", type: "submit", text: t("登录") }),
      el("button", { class: "ghost", type: "button", text: t("注册新账号") }),
    ),
  );
  view.append(card);
  document.body.append(view);

  const settle = async (mode: "login" | "register"): Promise<void> => {
    errorSlot.replaceChildren();
    setServerBase(server.value);
    const action = mode === "login" ? loginAccount : registerAccount;
    const primary = card.querySelector<HTMLButtonElement>(".login-primary");
    const secondary = card.querySelector<HTMLButtonElement>(".ghost");
    for (const item of [primary, secondary]) if (item !== null) item.disabled = true;
    try {
      const outcome = await action({
        username: username.value.trim(),
        password: password.value,
        device: device.value,
      });
      localStorage.setItem("joy-ops-token", outcome.token);
      view.remove();
      request.onAuthed();
    } catch (error) {
      errorSlot.append(failure(error as { code: number; message: string }));
    } finally {
      for (const item of [primary, secondary]) if (item !== null) item.disabled = false;
    }
  };

  card.addEventListener("submit", (event) => {
    event.preventDefault();
    void settle("login");
  });
  card
    .querySelector(".ghost")
    ?.addEventListener("click", () => void settle("register"));

  username.focus();
}
