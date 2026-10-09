<script>
  import { navGroups } from './data.js';
  import { activeSection } from './stores.js';
  export let open = false;
  export let onNavigate = () => {};
</script>

<aside class="side" class:open>
  {#each navGroups as group}
    <h5>{group.title}</h5>
    {#each group.items as item}
      <a
        href={'#' + item.id}
        class:active={$activeSection === item.id}
        on:click={onNavigate}>{item.label}</a>
    {/each}
  {/each}
</aside>
