// Custom element powering the template editor: highlights which inferred data
// fields are present in the JSON the user types. Loaded from /assets so the page
// needs no inline script (keeps script-src 'self' in the CSP).
if (!customElements.get('template-detail-page')) {
  customElements.define('template-detail-page', class extends HTMLElement {
    connectedCallback() {
      this.input = this.querySelector('[data-json-input]');
      this.fields = Array.from(this.querySelectorAll('[data-data-field]'));
      this.onInput = () => this.updateFields();
      this.input?.addEventListener('input', this.onInput);
      this.updateFields();
    }

    disconnectedCallback() {
      this.input?.removeEventListener('input', this.onInput);
    }

    updateFields() {
      let data = null;
      try {
        data = JSON.parse(this.input?.value || '{}');
      } catch (_) {
        data = null;
      }

      for (const field of this.fields) {
        const path = field.dataset.dataField || '';
        const used = data !== null && this.hasValueAtPath(data, path);
        field.toggleAttribute('data-used', used);
      }
    }

    hasValueAtPath(data, path) {
      const value = path.split('.').filter(Boolean).reduce((cursor, part) => {
        if (cursor && typeof cursor === 'object' && Object.hasOwn(cursor, part)) {
          return cursor[part];
        }
        return undefined;
      }, data);
      return this.hasMeaningfulValue(value);
    }

    hasMeaningfulValue(value) {
      if (value === undefined || value === null) return false;
      if (typeof value === 'string') return value.trim().length > 0;
      if (Array.isArray(value)) return value.some((item) => this.hasMeaningfulValue(item));
      if (typeof value === 'object') {
        return Object.values(value).some((item) => this.hasMeaningfulValue(item));
      }
      return true;
    }
  });
}
