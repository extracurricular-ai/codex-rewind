document.documentElement.classList.add("js");

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
    button.textContent = "Copy";
    button.setAttribute(
      "aria-label",
      `Copy ${block.querySelector(".code-header span").textContent} code`,
    );
    button.setAttribute("aria-live", "polite");
    block.querySelector(".code-header").append(button);
    button.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(
          block.querySelector("code").textContent,
        );
        button.textContent = "Copied";
      } catch {
        button.textContent = "Select text to copy";
      }
      setTimeout(() => {
        button.textContent = "Copy";
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
    ? "back in the composer"
    : "completed";
  document.querySelector("#demo-status").textContent = rewound
    ? "Rewound to before “Add greetings”. The file has one line; the prompt is a draft."
    : "Redo complete. The conversation and both lines are back.";
  rewind.disabled = rewound;
  redo.disabled = !rewound;
  (rewound ? redo : rewind).focus({ preventScroll: true });
}
rewind.addEventListener("click", () => showRewind(true));
redo.addEventListener("click", () => showRewind(false));
