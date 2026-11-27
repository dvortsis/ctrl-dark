try {
  const meta = document.querySelector('meta[name="color-scheme"]');
  const saved = localStorage.getItem('color-scheme');
  if (meta && (meta.content === 'light dark' || !meta.content) && saved) {
    meta.content = saved;
  }
} catch (_) {}
