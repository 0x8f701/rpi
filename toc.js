// Populate the sidebar
//
// This is a script, and not included directly in the page, to control the total size of the book.
// The TOC contains an entry for each page, so if each page includes a copy of the TOC,
// the total size of the page becomes O(n**2).
class MDBookSidebarScrollbox extends HTMLElement {
    constructor() {
        super();
    }
    connectedCallback() {
        this.innerHTML = '<ol class="chapter"><li class="chapter-item expanded affix "><a href="index.html">Introduction</a></li><li class="chapter-item expanded affix "><li class="part-title">Introduction</li><li class="chapter-item expanded "><a href="introduction/quickstart.html"><strong aria-hidden="true">1.</strong> Quick start</a></li><li class="chapter-item expanded "><a href="introduction/install.html"><strong aria-hidden="true">2.</strong> Installation</a></li><li class="chapter-item expanded affix "><li class="part-title">User Guide</li><li class="chapter-item expanded "><a href="user-guide/tui.html"><strong aria-hidden="true">3.</strong> TUI and keybindings</a></li><li class="chapter-item expanded "><a href="user-guide/cli-modes.html"><strong aria-hidden="true">4.</strong> CLI modes and commands</a></li><li class="chapter-item expanded "><a href="user-guide/goals.html"><strong aria-hidden="true">5.</strong> Goals</a></li><li class="chapter-item expanded "><a href="user-guide/todos.html"><strong aria-hidden="true">6.</strong> Todos and the Todo DAG</a></li><li class="chapter-item expanded "><a href="user-guide/orchestration.html"><strong aria-hidden="true">7.</strong> Orchestration: subagents, jobs, and IRC</a></li><li class="chapter-item expanded "><a href="user-guide/workflows.html"><strong aria-hidden="true">8.</strong> Isolated concurrent workflows</a></li><li class="chapter-item expanded "><a href="user-guide/session-recovery.html"><strong aria-hidden="true">9.</strong> Session recovery: rewind, checkpoints, handoffs, and TTL</a></li><li class="chapter-item expanded "><a href="user-guide/live.html"><strong aria-hidden="true">10.</strong> Live voice (/live)</a></li><li class="chapter-item expanded "><a href="user-guide/models.html"><strong aria-hidden="true">11.</strong> Models and custom providers</a></li><li class="chapter-item expanded "><a href="user-guide/authentication.html"><strong aria-hidden="true">12.</strong> Authentication</a></li><li class="chapter-item expanded "><a href="user-guide/rpc-json.html"><strong aria-hidden="true">13.</strong> RPC JSONL protocol</a></li><li class="chapter-item expanded "><a href="user-guide/web.html"><strong aria-hidden="true">14.</strong> Web client (/web)</a></li><li class="chapter-item expanded "><a href="user-guide/e2e-scenarios.html"><strong aria-hidden="true">15.</strong> E2E scenarios (user-perspective tmux tests)</a></li><li class="chapter-item expanded affix "><li class="part-title">Reference</li><li class="chapter-item expanded "><a href="reference/settings-trust.html"><strong aria-hidden="true">16.</strong> Settings, configuration, and trust</a></li><li class="chapter-item expanded "><a href="reference/architecture.html"><strong aria-hidden="true">17.</strong> Architecture</a></li><li class="chapter-item expanded "><a href="reference/configuration-profiles.html"><strong aria-hidden="true">18.</strong> Configuration profiles, TOML settings, and scoped auth</a></li><li class="chapter-item expanded "><a href="reference/environment-variables.html"><strong aria-hidden="true">19.</strong> Environment variables</a></li><li class="chapter-item expanded "><a href="reference/sandbox-isolation.html"><strong aria-hidden="true">20.</strong> Sandbox and overlayfs isolation</a></li><li class="chapter-item expanded "><a href="reference/hooks.html"><strong aria-hidden="true">21.</strong> Hooks and trust hooks</a></li><li class="chapter-item expanded "><a href="reference/extensions.html"><strong aria-hidden="true">22.</strong> Extensions and process protocol</a></li><li class="chapter-item expanded "><a href="reference/skills.html"><strong aria-hidden="true">23.</strong> Skills</a></li><li class="chapter-item expanded "><a href="reference/packages.html"><strong aria-hidden="true">24.</strong> Packages</a></li><li class="chapter-item expanded "><a href="reference/memory.html"><strong aria-hidden="true">25.</strong> Memory</a></li><li class="chapter-item expanded "><a href="reference/tools.html"><strong aria-hidden="true">26.</strong> Extended tool catalog</a></li><li class="chapter-item expanded "><a href="reference/mcp.html"><strong aria-hidden="true">27.</strong> Model Context Protocol (MCP) client</a></li><li class="chapter-item expanded "><a href="reference/acp.html"><strong aria-hidden="true">28.</strong> Agent Client Protocol (ACP) mode</a></li><li class="chapter-item expanded "><a href="reference/prompt-templates.html"><strong aria-hidden="true">29.</strong> Prompt templates and system prompt assembly</a></li><li class="chapter-item expanded "><a href="reference/local-llama.html"><strong aria-hidden="true">30.</strong> Local / self-hosted models with llama.cpp</a></li><li class="chapter-item expanded "><a href="reference/security.html"><strong aria-hidden="true">31.</strong> Security</a></li><li class="chapter-item expanded "><a href="reference/export-share.html"><strong aria-hidden="true">32.</strong> Export and share</a></li><li class="chapter-item expanded "><a href="reference/update.html"><strong aria-hidden="true">33.</strong> Update safety</a></li></ol>';
        // Set the current, active page, and reveal it if it's hidden
        let current_page = document.location.href.toString().split("#")[0].split("?")[0];
        if (current_page.endsWith("/")) {
            current_page += "index.html";
        }
        var links = Array.prototype.slice.call(this.querySelectorAll("a"));
        var l = links.length;
        for (var i = 0; i < l; ++i) {
            var link = links[i];
            var href = link.getAttribute("href");
            if (href && !href.startsWith("#") && !/^(?:[a-z+]+:)?\/\//.test(href)) {
                link.href = path_to_root + href;
            }
            // The "index" page is supposed to alias the first chapter in the book.
            if (link.href === current_page || (i === 0 && path_to_root === "" && current_page.endsWith("/index.html"))) {
                link.classList.add("active");
                var parent = link.parentElement;
                if (parent && parent.classList.contains("chapter-item")) {
                    parent.classList.add("expanded");
                }
                while (parent) {
                    if (parent.tagName === "LI" && parent.previousElementSibling) {
                        if (parent.previousElementSibling.classList.contains("chapter-item")) {
                            parent.previousElementSibling.classList.add("expanded");
                        }
                    }
                    parent = parent.parentElement;
                }
            }
        }
        // Track and set sidebar scroll position
        this.addEventListener('click', function(e) {
            if (e.target.tagName === 'A') {
                sessionStorage.setItem('sidebar-scroll', this.scrollTop);
            }
        }, { passive: true });
        var sidebarScrollTop = sessionStorage.getItem('sidebar-scroll');
        sessionStorage.removeItem('sidebar-scroll');
        if (sidebarScrollTop) {
            // preserve sidebar scroll position when navigating via links within sidebar
            this.scrollTop = sidebarScrollTop;
        } else {
            // scroll sidebar to current active section when navigating via "next/previous chapter" buttons
            var activeSection = document.querySelector('#sidebar .active');
            if (activeSection) {
                activeSection.scrollIntoView({ block: 'center' });
            }
        }
        // Toggle buttons
        var sidebarAnchorToggles = document.querySelectorAll('#sidebar a.toggle');
        function toggleSection(ev) {
            ev.currentTarget.parentElement.classList.toggle('expanded');
        }
        Array.from(sidebarAnchorToggles).forEach(function (el) {
            el.addEventListener('click', toggleSection);
        });
    }
}
window.customElements.define("mdbook-sidebar-scrollbox", MDBookSidebarScrollbox);
