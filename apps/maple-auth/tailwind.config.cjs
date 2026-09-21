/** @type {import('tailwindcss').Config} */
module.exports = {
  content: [
    "./index.html",
    "./src/**/*.{ts,tsx}",
    "!./src/**/*.test.{ts,tsx}",
    "!./src/**/fixtures/**"
  ],
  darkMode: "class",
  prefix: "",
  theme: {
    container: {
      center: true,
      padding: "2rem",
      screens: {
        "2xl": "1400px"
      }
    },
    extend: {
      screens: {
        "landscape-short": { raw: "(orientation: landscape) and (max-height: 500px)" }
      },
      fontFamily: {
        sans: ["var(--app-font-family)"],
        display: ["Array", "sans-serif"],
        displayWide: ["Array Wide", "Array", "sans-serif"],
        mondwest: ["Mondwest", "sans-serif"]
      },
      colors: {
        border: "hsl(var(--border))",
        input: "hsl(var(--input))",
        ring: "hsl(var(--ring))",
        background: "hsl(var(--background))",
        foreground: "hsl(var(--foreground))",
        primary: {
          DEFAULT: "hsl(var(--primary))",
          foreground: "hsl(var(--primary-foreground))"
        },
        secondary: {
          DEFAULT: "hsl(var(--secondary))",
          foreground: "hsl(var(--secondary-foreground))"
        },
        destructive: {
          DEFAULT: "hsl(var(--destructive))",
          foreground: "hsl(var(--destructive-foreground))",
          onFilled: "hsl(var(--destructive-on-filled))"
        },
        muted: {
          DEFAULT: "hsl(var(--muted))",
          foreground: "hsl(var(--muted-foreground))"
        },
        accent: {
          DEFAULT: "hsl(var(--accent))",
          foreground: "hsl(var(--accent-foreground))"
        },
        popover: {
          DEFAULT: "hsl(var(--popover))",
          foreground: "hsl(var(--popover-foreground))"
        },
        card: {
          DEFAULT: "hsl(var(--card))",
          foreground: "hsl(var(--card-foreground))"
        },
        maple: {
          primary: {
            DEFAULT: "hsl(var(--maple-primary))",
            on: "hsl(var(--maple-on-primary))",
            container: "hsl(var(--maple-primary-container))",
            strong: "hsl(var(--maple-primary-strong))"
          },
          secondary: {
            DEFAULT: "hsl(var(--maple-secondary))",
            on: "hsl(var(--maple-on-secondary))",
            container: "hsl(var(--maple-secondary-container))",
            700: "hsl(var(--maple-secondary-700))"
          },
          tertiary: {
            DEFAULT: "hsl(var(--maple-tertiary))",
            on: "hsl(var(--maple-on-tertiary))",
            container: "hsl(var(--maple-tertiary-container))"
          },
          success: "hsl(var(--maple-success))",
          warning: "hsl(var(--maple-warning))",
          onWarning: "hsl(var(--maple-on-warning))",
          error: "hsl(var(--maple-error))",
          info: "hsl(var(--maple-info))",
          surface: {
            DEFAULT: "hsl(var(--maple-surface))",
            dim: "hsl(var(--maple-surface-dim))"
          }
        },
        marketingNav: {
          bg: "hsl(var(--marketing-nav-bg))",
          fg: "hsl(var(--marketing-nav-fg))"
        },
        /* Matches design neutral swatches (#FAFAFA … #0A0A0A); see index.css */
        neutral: {
          50: "hsl(var(--neutral-50) / <alpha-value>)",
          100: "hsl(var(--neutral-100) / <alpha-value>)",
          200: "hsl(var(--neutral-200) / <alpha-value>)",
          300: "hsl(var(--neutral-300) / <alpha-value>)",
          400: "hsl(var(--neutral-400) / <alpha-value>)",
          500: "hsl(var(--neutral-500) / <alpha-value>)",
          600: "hsl(var(--neutral-600) / <alpha-value>)",
          700: "hsl(var(--neutral-700) / <alpha-value>)",
          800: "hsl(var(--neutral-800) / <alpha-value>)",
          900: "hsl(var(--neutral-900) / <alpha-value>)",
          950: "hsl(var(--neutral-950) / <alpha-value>)"
        }
      },
      borderRadius: {
        lg: "var(--radius)",
        md: "calc(var(--radius) - 2px)",
        sm: "calc(var(--radius) - 4px)"
      },
      keyframes: {
        "accordion-down": {
          from: { height: "0" },
          to: { height: "var(--radix-accordion-content-height)" }
        },
        "accordion-up": {
          from: { height: "var(--radix-accordion-content-height)" },
          to: { height: "0" }
        },
        shimmer: {
          "100%": {
            transform: "translateX(100%)"
          }
        }
      },
      animation: {
        "accordion-down": "accordion-down 0.2s ease-out",
        "accordion-up": "accordion-up 0.2s ease-out",
        shimmer: "shimmer 2s infinite"
      }
    }
  },
  plugins: [require("tailwindcss-animate")]
};
