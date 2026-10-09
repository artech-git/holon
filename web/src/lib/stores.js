import { writable } from 'svelte/store';

// Which section is currently in view — driven by the IntersectionObserver in
// App.svelte, read by the Sidebar to highlight the active link.
export const activeSection = writable('overview');
