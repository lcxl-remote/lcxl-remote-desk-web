import { defineConfig } from 'vitepress'
import { withMermaid } from 'vitepress-plugin-mermaid'

// GitHub repository the open-source `web` project mirrors to.
const REPO_URL = 'https://github.com/lcxl/lcxl-remote-desk-web'

// When publishing to a GitHub Pages *project* site
// (https://<user>.github.io/<repo>/), set DOCS_BASE to '/<repo>/'.
// For a user/org page or a custom domain, keep the default '/'.
const base = process.env.DOCS_BASE ?? '/'

const enNav = [
  { text: 'Guide', link: '/guide/introduction' },
  { text: 'Features', link: '/features/streaming' },
  { text: 'Configuration', link: '/config/config-toml' },
  { text: 'Security', link: '/security/ai-security-model' },
  { text: 'Reference', link: '/reference/architecture' },
]

const enSidebar = [
  {
    text: 'Guide',
    items: [
      { text: 'Introduction', link: '/guide/introduction' },
      { text: 'Quick Start', link: '/guide/quick-start' },
      { text: 'Core Concepts', link: '/guide/concepts' },
      { text: 'Startup Modes', link: '/guide/startup-modes' },
      { text: 'Deployment', link: '/guide/deployment' },
    ],
  },
  {
    text: 'Features',
    items: [
      { text: 'Remote Control & Streaming', link: '/features/streaming' },
      { text: 'AI Diagnostics', link: '/features/ai-diagnostics' },
      { text: 'MCP Server', link: '/features/mcp-server' },
      { text: 'Terminal, Files & Clipboard', link: '/features/terminal-files-clipboard' },
      { text: 'Virtual Display', link: '/features/virtual-display' },
      { text: 'Privacy Screen & Whiteboard', link: '/features/privacy-whiteboard' },
    ],
  },
  {
    text: 'Configuration',
    items: [
      { text: 'config.toml Reference', link: '/config/config-toml' },
      { text: 'CLI Arguments', link: '/config/cli' },
    ],
  },
  {
    text: 'Security',
    items: [
      { text: 'AI Security Model', link: '/security/ai-security-model' },
      { text: 'Signaling Authentication', link: '/security/signaling-auth' },
      { text: 'Vulnerability Disclosure', link: '/security/disclosure' },
    ],
  },
  {
    text: 'Reference',
    items: [
      { text: 'Architecture', link: '/reference/architecture' },
      { text: 'Module Map', link: '/reference/modules' },
      { text: 'REST API', link: '/reference/api' },
      { text: 'Signaling Protocol', link: '/reference/signaling-protocol' },
      { text: 'Contributing', link: '/reference/contributing' },
    ],
  },
]

const zhNav = [
  { text: '指南', link: '/zh/guide/introduction' },
  { text: '功能', link: '/zh/features/streaming' },
  { text: '配置', link: '/zh/config/config-toml' },
  { text: '安全', link: '/zh/security/ai-security-model' },
  { text: '参考', link: '/zh/reference/architecture' },
]

const zhSidebar = [
  {
    text: '指南',
    items: [
      { text: '介绍', link: '/zh/guide/introduction' },
      { text: '快速开始', link: '/zh/guide/quick-start' },
      { text: '核心概念', link: '/zh/guide/concepts' },
      { text: '启动模式', link: '/zh/guide/startup-modes' },
      { text: '部署', link: '/zh/guide/deployment' },
    ],
  },
  {
    text: '功能',
    items: [
      { text: '远程控制与串流', link: '/zh/features/streaming' },
      { text: 'AI 诊断', link: '/zh/features/ai-diagnostics' },
      { text: 'MCP 服务', link: '/zh/features/mcp-server' },
      { text: '终端 / 文件 / 剪贴板', link: '/zh/features/terminal-files-clipboard' },
      { text: '虚拟显示器', link: '/zh/features/virtual-display' },
      { text: '防窥屏与白板', link: '/zh/features/privacy-whiteboard' },
    ],
  },
  {
    text: '配置',
    items: [
      { text: 'config.toml 参考', link: '/zh/config/config-toml' },
      { text: 'CLI 参数', link: '/zh/config/cli' },
    ],
  },
  {
    text: '安全',
    items: [
      { text: 'AI 安全模型', link: '/zh/security/ai-security-model' },
      { text: '信令鉴权', link: '/zh/security/signaling-auth' },
      { text: '漏洞披露', link: '/zh/security/disclosure' },
    ],
  },
  {
    text: '参考',
    items: [
      { text: '架构总览', link: '/zh/reference/architecture' },
      { text: '模块地图', link: '/zh/reference/modules' },
      { text: 'REST API', link: '/zh/reference/api' },
      { text: '信令协议', link: '/zh/reference/signaling-protocol' },
      { text: '贡献指南', link: '/zh/reference/contributing' },
    ],
  },
]

export default withMermaid(
  defineConfig({
    base,
    title: 'LCXL Remote Desk',
    description:
      'An AI-native, open-source high-performance WebRTC remote desktop.',
    lastUpdated: true,
    cleanUrls: true,
    metaChunk: true,

    // Keep the repo's own README out of the published site.
    srcExclude: ['README.md'],

    head: [['link', { rel: 'icon', href: `${base}favicon.svg` }]],

    themeConfig: {
      socialLinks: [{ icon: 'github', link: REPO_URL }],
      search: { provider: 'local' },
    },

    locales: {
      root: {
        label: 'English',
        lang: 'en-US',
        themeConfig: {
          nav: enNav,
          sidebar: enSidebar,
          editLink: {
            pattern: `${REPO_URL}/edit/main/docs/:path`,
            text: 'Edit this page on GitHub',
          },
        },
      },
      zh: {
        label: '简体中文',
        lang: 'zh-CN',
        link: '/zh/',
        themeConfig: {
          nav: zhNav,
          sidebar: zhSidebar,
          outline: { label: '本页目录' },
          docFooter: { prev: '上一页', next: '下一页' },
          lastUpdatedText: '最后更新',
          returnToTopLabel: '返回顶部',
          sidebarMenuLabel: '菜单',
          darkModeSwitchLabel: '外观',
          editLink: {
            pattern: `${REPO_URL}/edit/main/docs/:path`,
            text: '在 GitHub 上编辑此页',
          },
        },
      },
    },
  }),
)
