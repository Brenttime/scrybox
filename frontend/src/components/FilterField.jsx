const labelStyle = { fontSize: '0.72rem', fontWeight: 700, color: 'var(--text-muted)', textTransform: 'uppercase', letterSpacing: '0.03em' };

// Small labelled field wrapper to keep the filter grids uniform
// (Collection Filters and Add Cards > Search Filters).
export default function FilterField({ label, children, style }) {
  return (
    <div className="form-group" style={{ marginBottom: 0, ...style }}>
      <label style={labelStyle}>{label}</label>
      {children}
    </div>
  );
}
