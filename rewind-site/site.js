document.documentElement.classList.add("js");

const chinese = document.documentElement.lang === "zh-CN";
const labels = chinese
  ? {
      copy: "复制",
      copied: "已复制",
      copyFailed: "请选中文字复制",
      draft: "已回到输入框",
      completed: "已完成",
      rewound: "已回退到“添加问候语”之前。文件只剩一行，提示词已回到草稿。",
      redone: "已撤销回退。对话和两行文件内容都恢复了。",
    }
  : {
      copy: "Copy",
      copied: "Copied",
      copyFailed: "Select text to copy",
      draft: "back in the composer",
      completed: "completed",
      rewound:
        "Rewound to before “Add greetings”. The file has one line; the prompt is a draft.",
      redone: "Redo complete. The conversation and both lines are back.",
    };

const languageSwitch = document.querySelector("[data-language-switch]");
languageSwitch.addEventListener("click", () => {
  languageSwitch.hash = document.querySelector(".nav-link[aria-current]").hash;
});

const menu = document.querySelector(".menu-toggle");
const sidebar = document.querySelector(".sidebar");
const links = [...document.querySelectorAll(".nav-link")];
menu.hidden = false;

function closeMenu() {
  menu.setAttribute("aria-expanded", "false");
  sidebar.classList.remove("is-open");
}

menu.addEventListener("click", () => {
  const open = menu.getAttribute("aria-expanded") !== "true";
  menu.setAttribute("aria-expanded", String(open));
  sidebar.classList.toggle("is-open", open);
});

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && menu.getAttribute("aria-expanded") === "true") {
    closeMenu();
    menu.focus();
  }
});

links.forEach((link) => {
  link.addEventListener("click", () => {
    if (menu.getAttribute("aria-expanded") === "true") {
      closeMenu();
      const section = document.querySelector(link.hash);
      section.tabIndex = -1;
      section.focus({ preventScroll: true });
    }
  });
});

const sections = [...document.querySelectorAll("main > section")];
let scheduled = false;
function updateNavigation() {
  const current =
    sections
      .filter((section) => section.getBoundingClientRect().top <= 140)
      .at(-1) || sections[0];
  links.forEach((link) => {
    if (link.hash === `#${current.id}`)
      link.setAttribute("aria-current", "location");
    else link.removeAttribute("aria-current");
  });
  scheduled = false;
}
window.addEventListener(
  "scroll",
  () => {
    if (!scheduled) {
      scheduled = true;
      requestAnimationFrame(updateNavigation);
    }
  },
  { passive: true },
);
window.addEventListener("resize", updateNavigation);
updateNavigation();

if (navigator.clipboard && window.isSecureContext) {
  document.querySelectorAll(".code-block").forEach((block) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "copy-button";
    button.textContent = labels.copy;
    button.setAttribute(
      "aria-label",
      chinese
        ? `复制${block.querySelector(".code-header span").textContent}代码`
        : `Copy ${block.querySelector(".code-header span").textContent} code`,
    );
    button.setAttribute("aria-live", "polite");
    block.querySelector(".code-header").append(button);
    button.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(
          block.querySelector("code").textContent,
        );
        button.textContent = labels.copied;
      } catch {
        button.textContent = labels.copyFailed;
      }
      setTimeout(() => {
        button.textContent = labels.copy;
      }, 2000);
    });
  });
}

const rewind = document.querySelector("#rewind-demo");
const redo = document.querySelector("#redo-demo");
document.querySelector(".demo-controls").hidden = false;
function showRewind(rewound) {
  document.querySelector("#greeting-line").hidden = rewound;
  document
    .querySelector("#greeting-turn")
    .classList.toggle("is-rewound", rewound);
  document.querySelector("#turn-state").textContent = rewound
    ? labels.draft
    : labels.completed;
  document.querySelector("#demo-status").textContent = rewound
    ? labels.rewound
    : labels.redone;
  rewind.disabled = rewound;
  redo.disabled = !rewound;
  (rewound ? redo : rewind).focus({ preventScroll: true });
}
rewind.addEventListener("click", () => showRewind(true));
redo.addEventListener("click", () => showRewind(false));
