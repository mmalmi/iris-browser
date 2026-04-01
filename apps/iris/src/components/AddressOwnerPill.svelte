<script lang="ts">
  import { createAddressOwnerStore } from '../lib/addressIdentity';

  type OwnerLabelSize = 'sm' | 'xs';

  interface Props {
    host: string;
    openProfile?: () => void;
    interactive?: boolean;
    showBackground?: boolean;
    maxWidthClass?: string;
    allowShrink?: boolean;
    size?: OwnerLabelSize;
    testId?: string;
  }

  let {
    host,
    openProfile,
    interactive = true,
    showBackground = true,
    maxWidthClass = 'max-w-28',
    allowShrink = false,
    size = 'sm',
    testId,
  }: Props = $props();

  let ownerStore = $derived(createAddressOwnerStore(host));
  let owner = $derived($ownerStore);
  let canOpenProfile = $derived(interactive && !!openProfile);
  let imgError = $state(false);

  let sizeConfig = $derived(size === 'xs'
    ? {
        root: 'gap-1 pl-0 pr-1.5 py-0',
        avatar: 'h-4 w-4',
        avatarSize: 16,
        badge: 'h-3 w-3',
        icon: 7,
        text: 'text-[11px]',
      }
    : {
        root: 'gap-1 px-1 py-0.5',
        avatar: 'h-4 w-4',
        avatarSize: 16,
        badge: 'h-3 w-3',
        icon: 7,
        text: 'text-xs',
      });

  let rootClass = $derived.by(() => {
    const backgroundClass = showBackground ? 'rounded-full bg-surface-2/85 hover:bg-surface-3' : '';
    const interactiveClass = canOpenProfile ? 'transition-colors' : '';
    const sizingClass = allowShrink ? 'min-w-0' : 'shrink-0';
    return `inline-flex ${sizingClass} items-center ${sizeConfig.root} ${maxWidthClass} ${backgroundClass} ${interactiveClass}`.trim();
  });

  $effect(() => {
    host;
    owner.avatarUrl;
    imgError = false;
  });

  function handleMouseDown(event: MouseEvent) {
    if (!canOpenProfile) return;
    event.preventDefault();
    event.stopPropagation();
  }

  function handleClick(event: MouseEvent) {
    if (!canOpenProfile) return;
    event.preventDefault();
    event.stopPropagation();
    openProfile?.();
  }
</script>

{#if canOpenProfile}
  <button
    type="button"
    data-testid={testId}
    data-profile-url={owner.profileUrl}
    class={`relative z-20 text-left text-text-1 leading-none ${rootClass}`}
    title={owner.host}
    aria-label={`Open ${owner.name} profile`}
    onmousedown={handleMouseDown}
    onclick={handleClick}
  >
    <span class="relative shrink-0">
      <img
        data-testid={testId ? 'address-owner-avatar' : undefined}
        src={imgError ? owner.fallbackAvatarUrl : owner.avatarUrl}
        alt=""
        width={sizeConfig.avatarSize}
        height={sizeConfig.avatarSize}
        class={`rounded-full object-cover ${sizeConfig.avatar}`}
        onerror={() => (imgError = true)}
      />
      {#if owner.showBadge}
        <span
          data-testid={testId ? 'address-owner-badge' : undefined}
          class={`absolute -right-0.5 -top-0.5 flex items-center justify-center rounded-full bg-accent text-white shadow-sm ${sizeConfig.badge}`}
        >
          <svg width={sizeConfig.icon} height={sizeConfig.icon} viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path
              d="M20 6L9 17L4 12"
              stroke="currentColor"
              stroke-width="3"
              stroke-linecap="round"
              stroke-linejoin="round"
            />
          </svg>
        </span>
      {/if}
    </span>
    <span
      data-testid={testId ? 'address-owner-name' : undefined}
      class={`min-w-0 truncate font-medium leading-none ${sizeConfig.text} ${owner.isFallbackName ? 'italic opacity-70' : ''}`}
    >
      {owner.name}
    </span>
  </button>
{:else}
  <span
    class={`text-left text-text-1 leading-none ${rootClass}`}
    title={owner.host}
  >
    <span class="relative shrink-0">
      <img
        src={imgError ? owner.fallbackAvatarUrl : owner.avatarUrl}
        alt=""
        width={sizeConfig.avatarSize}
        height={sizeConfig.avatarSize}
        class={`rounded-full object-cover ${sizeConfig.avatar}`}
        onerror={() => (imgError = true)}
      />
      {#if owner.showBadge}
        <span
          class={`absolute -right-0.5 -top-0.5 flex items-center justify-center rounded-full bg-accent text-white shadow-sm ${sizeConfig.badge}`}
        >
          <svg width={sizeConfig.icon} height={sizeConfig.icon} viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path
              d="M20 6L9 17L4 12"
              stroke="currentColor"
              stroke-width="3"
              stroke-linecap="round"
              stroke-linejoin="round"
            />
          </svg>
        </span>
      {/if}
    </span>
    <span class={`min-w-0 truncate font-medium leading-none ${sizeConfig.text} ${owner.isFallbackName ? 'italic opacity-70' : ''}`}>
      {owner.name}
    </span>
  </span>
{/if}
