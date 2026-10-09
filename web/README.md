# txp docs site

The documentation & usage-guide site for **txp**, built with [Svelte](https://svelte.dev)
and [Vite](https://vitejs.dev). Output is a fully static bundle, deployed to
GitHub Pages by [`.github/workflows/pages.yml`](../.github/workflows/pages.yml)
on every branch push.

## Develop

```sh
cd web
npm install
npm run dev        # http://localhost:5173
```

## Build

```sh
npm run build      # -> web/dist
npm run preview    # serve the built bundle locally
```

The GitHub Pages base path is configurable via the `BASE_PATH` env var and
defaults to `/holon/` (a project site at `https://<user>.github.io/holon/`).
CI sets it from the repository name automatically:

```sh
BASE_PATH=/holon/ npm run build
```

## Layout

```
web/
  index.html            Vite entry; sets the no-flash theme before paint
  vite.config.js        base path + Svelte plugin
  src/
    main.js             mounts <App>
    app.css             global design tokens + all component styles
    App.svelte          shell: theme, mobile nav, scrollspy
    lib/
      data.js           sidebar navigation model
      snippets.js       pre-formatted code-block contents
      stores.js         active-section store
      CodeBlock.svelte  code block + copy button (reusable)
      Callout.svelte    info / warn / tip callout (reusable)
      TopBar.svelte     sticky header
      Sidebar.svelte    section navigation
      Hero.svelte       landing hero + flow diagram
      Content.svelte    the 13 documentation sections
      Footer.svelte     footer
```
