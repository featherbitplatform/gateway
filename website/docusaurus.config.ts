import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

const config: Config = {
  title: 'featherbit',
  tagline:
    'A high-performance API gateway delivered as a single Rust binary. Routes are visual node graphs — plugins wired together through success and error ports — serving HTTP/1.1, HTTP/2, WebSocket, and raw TCP/UDP.',
  // The feather mark, padded onto a square canvas. Don't point this at
  // featherbit-mark.png directly: that one is a tall 511x853, and browsers
  // squash it to fit the square tab-icon box.
  favicon: 'img/featherbit-favicon.png',

  future: {
    v4: true,
  },

  url: 'https://francescodedomenico.github.io',
  baseUrl: '/featherbit/',
  organizationName: 'francescodedomenico',
  projectName: 'featherbit',
  trailingSlash: false,

  onBrokenLinks: 'throw',

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      {
        docs: {
          sidebarPath: './sidebars.ts',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    image: 'img/featherbit-mark.png',
    colorMode: {
      defaultMode: 'dark',
      respectPrefersColorScheme: true,
    },
    navbar: {
      title: 'featherbit',
      logo: {
        alt: 'featherbit logo',
        src: 'img/featherbit-mark.png',
      },
      items: [
        {
          type: 'docSidebar',
          sidebarId: 'docs',
          position: 'left',
          label: 'Docs',
        },
        {
          to: '/docs/reference/plugins',
          position: 'left',
          label: 'Plugins',
        },
        {
          type: 'dropdown',
          label: 'API',
          position: 'left',
          items: [
            {
              label: 'Rust internals (rustdoc)',
              href: 'pathname:///api/rust/featherbit/index.html',
            },
            {
              label: 'Admin UI (TypeDoc)',
              href: 'pathname:///api/ui/index.html',
            },
          ],
        },
        {
          href: 'https://github.com/francescodedomenico/featherbit',
          label: 'GitHub',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      links: [],
      copyright: `Apache-2.0 License · featherbit ${new Date().getFullYear()}`,
    },
    prism: {
      theme: prismThemes.oneLight,
      darkTheme: prismThemes.oneDark,
      additionalLanguages: ['rust', 'lua', 'yaml', 'bash', 'json', 'toml'],
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
