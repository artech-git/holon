<script>
  import { onMount } from 'svelte';
  import TopBar from './lib/TopBar.svelte';
  import Sidebar from './lib/Sidebar.svelte';
  import Hero from './lib/Hero.svelte';
  import Content from './lib/Content.svelte';
  import Footer from './lib/Footer.svelte';
  import { activeSection } from './lib/stores.js';

  let navOpen = false;
  // Initial theme comes from the inline <head> script (set before first paint).
  let theme =
    (typeof document !== 'undefined' && document.documentElement.getAttribute('data-theme')) ||
    'dark';

  function toggleTheme() {
    theme = theme === 'dark' ? 'light' : 'dark';
    document.documentElement.setAttribute('data-theme', theme);
    try {
      localStorage.setItem('txp-theme', theme);
    } catch (e) {}
  }

  function closeNav() {
    navOpen = false;
    document.body.classList.remove('no-scroll');
  }
  function toggleNav() {
    navOpen = !navOpen;
    document.body.classList.toggle('no-scroll', navOpen);
  }

  onMount(() => {
    const sections = Array.from(document.querySelectorAll('section[id]'));
    const obs = new IntersectionObserver(
      (entries) => {
        entries.forEach((en) => {
          if (en.isIntersecting) activeSection.set(en.target.id);
        });
      },
      { rootMargin: '-40% 0px -55% 0px', threshold: 0 }
    );
    sections.forEach((s) => obs.observe(s));
    return () => obs.disconnect();
  });
</script>

<TopBar {theme} onToggleTheme={toggleTheme} onToggleNav={toggleNav} />

<!-- svelte-ignore a11y-click-events-have-key-events a11y-no-static-element-interactions -->
<div class="scrim" class:show={navOpen} on:click={closeNav} aria-hidden="true"></div>

<span id="top"></span>
<Hero />

<div class="layout">
  <Sidebar open={navOpen} onNavigate={closeNav} />
  <main>
    <Content />
  </main>
</div>

<Footer />
