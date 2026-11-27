try {
  document.querySelector('meta[name="color-scheme"]').content =
    localStorage.getItem('color-scheme') ?? 'light dark';
} catch (_) {}
