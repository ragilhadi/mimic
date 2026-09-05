// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import sitemap from '@astrojs/sitemap';

export default defineConfig({
  site: 'https://mimic.ragilhadi.com',
  integrations: [
    sitemap(),
    starlight({
      title: 'Docs | Mimic',
      description:
        'A fast, lightweight HTTP mock server built with Rust and Axum. Define mock responses in simple JSON files.',
      favicon: '/favicon.svg',
      head: [
        { tag: 'meta', attrs: { name: 'theme-color', content: '#1a1a2e' } },
        { tag: 'meta', attrs: { name: 'author', content: 'Ragil Ilman' } },
        { tag: 'link', attrs: { rel: 'icon', type: 'image/svg+xml', href: '/favicon.svg' } },
        { tag: 'link', attrs: { rel: 'icon', type: 'image/png', sizes: '32x32', href: '/favicon.png' } },
        { tag: 'link', attrs: { rel: 'icon', type: 'image/x-icon', href: '/favicon.ico' } },
        { tag: 'link', attrs: { rel: 'apple-touch-icon', sizes: '180x180', href: '/apple-touch-icon.png' } },
        { tag: 'meta', attrs: { property: 'og:type', content: 'website' } },
        { tag: 'meta', attrs: { property: 'og:site_name', content: 'Mimic' } },
        { tag: 'meta', attrs: { property: 'og:image', content: 'https://mimic.ragilhadi.com/favicon.svg' } },
        { tag: 'meta', attrs: { property: 'og:image:width', content: '32' } },
        { tag: 'meta', attrs: { property: 'og:image:height', content: '32' } },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary' } },
        { tag: 'meta', attrs: { name: 'twitter:title', content: 'Mimic — Fast HTTP Mock Server' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: 'https://mimic.ragilhadi.com/favicon.svg' } },
      ],
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/ragilhadi/mimic',
        },
        {
          icon: 'seti:docker',
          label: 'Docker Hub',
          href: 'https://hub.docker.com/r/ragilhadi/mimic',
        },
      ],
      editLink: {
        baseUrl: 'https://github.com/ragilhadi/mimic/edit/main/docs/',
      },
      sidebar: [
        {
          label: 'Start Here',
          items: [
            { label: 'Introduction', slug: 'start-here/introduction' },
            { label: 'Quick Start', slug: 'start-here/quick-start' },
            { label: 'Installation', slug: 'start-here/installation' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Mock Files', slug: 'guides/mock-files' },
            { label: 'Hot Reload', slug: 'guides/hot-reload' },
            { label: 'File Uploads', slug: 'guides/file-uploads' },
            { label: 'Configuration', slug: 'guides/configuration' },
            { label: 'Scenario Tags', slug: 'guides/scenarios' },
            { label: 'Built-in CORS', slug: 'guides/cors' },
            { label: 'Proxy & Record-and-Replay', slug: 'guides/proxy' },
            { label: 'OpenAPI Import', slug: 'guides/openapi-import' },
            { label: 'Admin Dashboard', slug: 'guides/admin-dashboard' },
          ],
        },
        {
          label: 'Advanced Matching',
          items: [
            { label: 'Overview', slug: 'matching/overview' },
            { label: 'Path Parameters', slug: 'matching/path-parameters' },
            { label: 'Query Parameters', slug: 'matching/query-params' },
            { label: 'Headers', slug: 'matching/headers' },
            { label: 'Request Body', slug: 'matching/body' },
            { label: 'Trailing-Slash Tolerance', slug: 'matching/trailing-slash' },
            { label: 'Match Priority', slug: 'matching/priority' },
          ],
        },
        {
          label: 'Dynamic Responses',
          items: [
            { label: 'Response Templating', slug: 'dynamic-responses/templating' },
            { label: 'File-Backed Responses', slug: 'dynamic-responses/response-file' },
            { label: 'Custom Response Headers', slug: 'dynamic-responses/response-headers' },
            { label: 'Response Delays', slug: 'dynamic-responses/delays' },
            { label: 'Stateful Sequences', slug: 'dynamic-responses/sequences' },
          ],
        },
        {
          label: 'Deployment',
          items: [
            { label: 'Docker', slug: 'deployment/docker' },
            { label: 'Docker Compose', slug: 'deployment/docker-compose' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Mock File Schema', slug: 'reference/mock-schema' },
            { label: 'Environment Variables', slug: 'reference/environment' },
            { label: 'Admin API', slug: 'reference/admin-api' },
            { label: 'Reserved Endpoints', slug: 'reference/reserved-endpoints' },
          ],
        },
      ],
    }),
  ],
});
