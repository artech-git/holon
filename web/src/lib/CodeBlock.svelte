<script>
  export let lang = '';
  export let file = '';
  export let code = '';

  let codeEl;
  let label = 'copy';

  function copy() {
    if (!codeEl) return;
    const text = codeEl.innerText;
    navigator.clipboard
      .writeText(text)
      .then(() => {
        label = 'copied';
        setTimeout(() => (label = 'copy'), 1400);
      })
      .catch(() => {});
  }
</script>

<div class="code">
  <div class="chead">
    <span class="lang">{lang}</span>
    {#if file}<span class="fn">{file}</span>{/if}
    <button class="copy" on:click={copy}>{label}</button>
  </div>
  <pre><code bind:this={codeEl}>{@html code}</code></pre>
</div>
