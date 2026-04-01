import { defineConfig, presetUno, presetIcons } from 'unocss';

export default defineConfig({
  safelist: [
    'i-lucide-chevron-left',
    'i-lucide-chevron-right',
    'i-lucide-home',
    'i-lucide-settings',
    'i-lucide-search',
    'i-lucide-loader-2',
    'i-lucide-refresh-cw',
    'i-lucide-star',
    'i-lucide-download',
    'i-lucide-clock',
    'i-lucide-x',
    'i-lucide-ellipsis',
    'i-lucide-user-round',
    // Settings navigation accent pills
    'bg-accent/8',
    'bg-accent/12',
    'text-accent',
    'ring-accent/20',
    'bg-rose-500/8',
    'bg-rose-500/12',
    'text-rose-500',
    'ring-rose-500/20',
    'bg-emerald-500/8',
    'bg-emerald-500/12',
    'text-emerald-500',
    'ring-emerald-500/20',
    'bg-sky-500/8',
    'bg-sky-500/12',
    'text-sky-500',
    'ring-sky-500/20',
    'bg-amber-500/10',
    'bg-amber-500/12',
    'text-amber-500',
    'ring-amber-500/20',
  ],
  presets: [
    presetUno(),
    presetIcons({
      scale: 1.2,
      extraProperties: {
        'display': 'inline-block',
        'vertical-align': 'middle',
      },
    }),
  ],
  theme: {
    colors: {
      surface: {
        0: 'rgb(var(--surface-0) / <alpha-value>)',
        1: 'rgb(var(--surface-1) / <alpha-value>)',
        2: 'rgb(var(--surface-2) / <alpha-value>)',
        3: 'rgb(var(--surface-3) / <alpha-value>)',
      },
      text: {
        1: 'rgb(var(--text-1) / <alpha-value>)',
        2: 'rgb(var(--text-2) / <alpha-value>)',
        3: 'rgb(var(--text-3) / <alpha-value>)',
      },
      accent: '#916dfe',
      success: '#2ba640',
      danger: '#ff0000',
      warning: '#ffcc00',
    },
    borderRadius: {
      DEFAULT: '6px',
      sm: '4px',
      lg: '8px',
    },
  },
  shortcuts: {
    'btn': 'px-3 py-1.5 min-h-9 rounded-full text-sm font-medium transition-colors duration-100 select-none disabled:opacity-50 disabled:cursor-not-allowed',
    'btn-ghost': 'btn bg-surface-2 text-text-1 hover:bg-surface-3 disabled:hover:bg-surface-2',
    'btn-circle': 'w-9 min-h-9 p-0! rounded-full flex items-center justify-center transition-colors duration-100 select-none outline-none focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-surface-1 [-webkit-tap-highlight-color:transparent] disabled:opacity-50 disabled:cursor-not-allowed',
    'input': 'px-3 py-1.5 bg-surface-0 b-1 b-solid b-surface-3 rounded-full text-text-1 outline-none focus:b-accent',
    'text-muted': 'text-text-2',
  },
  preflights: [
    {
      getCSS: () => `
        button {
          border: none;
          background: transparent;
          cursor: pointer;
          font: inherit;
          color: inherit;
        }
      `,
    },
  ],
});
