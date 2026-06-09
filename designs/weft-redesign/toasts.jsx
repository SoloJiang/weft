/* Toast / notification layer — transient feedback for actions (merge, create,
   delete, approve). In the real app this pairs with OS notifications when Weft
   is backgrounded and something needs you. */

const TOAST_KIND = {
  success: { Icon: IconCheck2, cls: "k-success" },
  info:    { Icon: IconRadio,  cls: "k-info" },
  warn:    { Icon: IconWarn,   cls: "k-warn" },
  error:   { Icon: IconX,      cls: "k-error" },
};

function Toast({ t, onDismiss }) {
  React.useEffect(() => {
    const id = setTimeout(() => onDismiss(t.id), t.action ? 7000 : 4500);
    return () => clearTimeout(id);
  }, []);
  const m = TOAST_KIND[t.kind] || TOAST_KIND.info;
  return (
    <div className={"toast " + m.cls}>
      <span className="toast-ico"><m.Icon size={15} /></span>
      <span className="toast-msg grow">{t.msg}</span>
      {t.action && <button className="btn btn-sm btn-default" onClick={() => { if (t.action.onClick) t.action.onClick(); onDismiss(t.id); }}>{t.action.label}</button>}
      <button className="btn-icon sm" onClick={() => onDismiss(t.id)}><IconX size={14} /></button>
    </div>
  );
}

function Toasts({ toasts, onDismiss }) {
  if (!toasts.length) return null;
  return <div className="toast-stack">{toasts.map((t) => <Toast key={t.id} t={t} onDismiss={onDismiss} />)}</div>;
}

Object.assign(window, { Toasts });
