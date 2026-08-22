# Leeway website

The marketing site is a static [Astro](https://astro.build/) project deployed
with Cloudflare Workers Static Assets. It has no Worker script or server-side
runtime; Cloudflare serves Astro's pre-rendered output directly.

## Local development

Run these commands from this directory:

```sh
npm ci
npm run dev
```

Create a production build with:

```sh
npm run build
```

Astro writes the static site to `dist/`.

## Cloudflare Workers setup

Import `nathanreyes/leeway` from GitHub in **Workers & Pages** and use these
build settings:

| Setting | Value |
| --- | --- |
| Project name | `leeway` |
| Build command | `npm run build` |
| Deploy command | `npx wrangler@latest deploy` |
| Non-production deploy command | `npx wrangler@latest versions upload` |
| Path | `/site` |

Leave build variables empty and leave Cloudflare Access off for the public
site. Builds for non-production branches may remain enabled. The selected API
token must have permission to deploy Workers.

The checked-in `wrangler.jsonc` points Workers Static Assets at Astro's `dist/`
output. No Astro Cloudflare adapter is required for this static build. The Node
requirement (`22.12.0` or newer) lives in `package.json`.

## Domain cutover

Verify the generated `workers.dev` deployment before changing production DNS.
Then open the Worker's **Settings > Domains & Routes**, add a custom domain,
and enter `get-leeway.com`. Let Cloudflare create the domain route before
changing or removing the old hosting record.

The domain is already Astro's canonical production URL in `astro.config.mjs`.
If `www.get-leeway.com` should also work, configure a Cloudflare redirect to
the apex domain.

After the custom domain is active and HTTPS works, the old Netlify site can be
disabled.
