# Rewind documentation site

This directory is a standalone static site. Edit `index.html`, `styles.css`, and
`site.js`; no build step, dependencies, or changes to the root workspace are needed.
Keep the documentation consistent with the project README when behavior changes.

English lives in `index.html`, and Simplified Chinese in `zh/index.html`. Update
both pages when documentation changes; keep their section IDs aligned so language
switching preserves the current section. Styles, scripts, and media are shared.
Interactive labels are localized in `site.js`. Both languages work without
JavaScript and link directly to each other, including on mobile.

Preview from the repository root:

```sh
python3 -m http.server 8000 --directory rewind-site
```

Open <http://localhost:8000>. Content and navigation work without JavaScript;
JavaScript adds copy buttons, mobile navigation, and the rewind illustration.

The Quickstart recording and walkthrough screenshots live in `assets/`, copied
from `.github/rewind.gif` and `.github/rewind-*.png`. When updating the original
recording or screenshots, refresh these copies too. The GIF plays only after the
reader expands the recording; the still screenshots link to their full-size files.

## Publishing

In GitHub repository **Settings → Pages**, select **GitHub Actions** as the source.
After the changes are pushed to `main`, `.github/workflows/rewind-pages.yml`
publishes this directory. The workflow can also be run manually from `main`.

Site URL: <https://extracurricular-ai.github.io/codex-rewind/>.

Assets use relative paths so the site works under the repository URL prefix.
Keep site changes in this directory and its dedicated workflow to minimize
overlap with upstream Codex changes. If you fork or rename the repository, update
the site’s repository links, this URL, and the links in both root READMEs.
