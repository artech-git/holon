<script>
  import { REPO_URL } from './data.js';
  export let theme = 'dark';
  export let onToggleTheme = () => {};
  export let onToggleNav = () => {};
</script>

<header class="top">
  <div class="wrap">
    <button class="iconbtn menu-toggle" aria-label="Toggle navigation" on:click={onToggleNav}>
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M3 6h18M3 12h18M3 18h18"/></svg>
    </button>
    <a class="brand" href="#top">
      <span class="logomark">tx</span>
      <span>txp <small>transactional processes</small></span>
    </a>
    <nav>
      <a class="hidemob" href="#quickstart">Quickstart</a>
      <a class="hidemob" href="#manifest">Manifest</a>
      <a class="hidemob" href="#cli">CLI</a>
      <a class="hidemob" href="#security">Security</a>
      <button class="iconbtn" aria-label="Toggle theme" title="Toggle theme" on:click={onToggleTheme}>
        {#if theme === 'dark'}
          <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/></svg>
        {:else}
          <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="4.2"/><path d="M12 2v2.5M12 19.5V22M2 12h2.5M19.5 12H22M4.6 4.6l1.8 1.8M17.6 17.6l1.8 1.8M19.4 4.6l-1.8 1.8M6.4 17.6l-1.8 1.8"/></svg>
        {/if}
      </button>
      <a class="ghbtn" href={REPO_URL} target="_blank" rel="noopener">
        <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor"><path d="M12 .5A11.5 11.5 0 0 0 .5 12a11.5 11.5 0 0 0 7.86 10.92c.57.1.78-.25.78-.55v-1.9c-3.2.7-3.88-1.54-3.88-1.54-.53-1.34-1.3-1.7-1.3-1.7-1.06-.72.08-.71.08-.71 1.17.08 1.79 1.2 1.79 1.2 1.04 1.79 2.73 1.27 3.4.97.1-.76.41-1.27.74-1.56-2.55-.29-5.23-1.28-5.23-5.7 0-1.26.45-2.29 1.19-3.1-.12-.29-.52-1.46.11-3.05 0 0 .97-.31 3.18 1.18a11 11 0 0 1 5.8 0c2.2-1.49 3.17-1.18 3.17-1.18.63 1.59.23 2.76.11 3.05.74.81 1.19 1.84 1.19 3.1 0 4.43-2.69 5.41-5.25 5.69.42.37.8 1.1.8 2.22v3.29c0 .3.21.66.79.55A11.5 11.5 0 0 0 23.5 12 11.5 11.5 0 0 0 12 .5z"/></svg>
        GitHub
      </a>
    </nav>
  </div>
</header>
