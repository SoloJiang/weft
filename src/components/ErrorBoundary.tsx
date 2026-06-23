import { Component, type ReactNode } from "react";
import { TriangleAlert } from "lucide-react";
import i18n from "../i18n";
import { StackTrace } from "./ai-elements";

/**
 * A screen crash shows a readable fallback instead of white-screening the whole
 * app (失败可读). Key it by route so navigating away clears the error.
 */
export class ErrorBoundary extends Component<
  { children: ReactNode },
  { err: Error | null }
> {
  state: { err: Error | null } = { err: null };

  static getDerivedStateFromError(err: Error) {
    return { err };
  }

  render() {
    if (!this.state.err) return this.props.children;
    return (
      <div className="flex min-w-0 flex-1 items-center justify-center bg-bg p-8">
        <div className="flex max-w-md flex-col items-center rounded-[var(--radius-lg)] border border-border bg-surface p-6 text-center">
          <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-md)] bg-danger/15 text-danger">
            <TriangleAlert size={20} />
          </div>
          <h2 className="mt-3 text-[14px] font-semibold text-ink">{i18n.t("error.title")}</h2>
          <p className="mt-1.5 text-[12px] text-ink-faint">{i18n.t("error.body")}</p>
          <StackTrace
            title={i18n.t("error.stackTrace")}
            message={String(this.state.err.message || this.state.err)}
            stack={this.state.err.stack}
          />
          <button
            onClick={() => this.setState({ err: null })}
            className="mt-3 rounded-[var(--radius-md)] bg-brand px-3 py-1.5 text-[12.5px] font-medium text-brand-ink transition-colors hover:bg-brand/90"
          >
            {i18n.t("error.retry")}
          </button>
        </div>
      </div>
    );
  }
}
