<script lang="ts">
  type Props = {
    class?: string;
    testId?: string;
  };

  let { class: className = '', testId }: Props = $props();
</script>

<svg
  viewBox="0 0 16 16"
  fill="none"
  aria-hidden="true"
  data-testid={testId}
  class={`inline-block shrink-0 animate-spin ${className}`}
  style="transform-box: fill-box; transform-origin: center;"
>
  <circle
    cx="8"
    cy="8"
    r="5.5"
    stroke="currentColor"
    stroke-width="1.5"
    stroke-opacity="0.25"
  />
  <path
    d="M13.5 8A5.5 5.5 0 0 0 8 2.5"
    stroke="currentColor"
    stroke-width="1.5"
    stroke-linecap="round"
  />
</svg>
