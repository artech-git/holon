// Sidebar navigation model. Each id must match a <section id="..."> in Content.svelte.
export const navGroups = [
  {
    title: 'Introduction',
    items: [
      { id: 'overview', label: 'Overview' },
      { id: 'guarantees', label: 'What is guaranteed' },
      { id: 'architecture', label: 'Architecture' }
    ]
  },
  {
    title: 'Guide',
    items: [
      { id: 'requirements', label: 'Requirements' },
      { id: 'quickstart', label: 'Quickstart' },
      { id: 'daemon', label: 'Running the daemon' }
    ]
  },
  {
    title: 'Reference',
    items: [
      { id: 'manifest', label: 'Manifest format' },
      { id: 'examples', label: 'Worked examples' },
      { id: 'cli', label: 'CLI commands' }
    ]
  },
  {
    title: 'Internals',
    items: [
      { id: 'protocol', label: 'The protocol' },
      { id: 'security', label: 'Security model' },
      { id: 'verification', label: 'Verification' }
    ]
  },
  {
    title: 'Project',
    items: [{ id: 'status', label: 'Status & roadmap' }]
  }
];

export const REPO_URL = 'https://github.com/artech-git/holon';
