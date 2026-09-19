import { createFileRoute, redirect } from "@tanstack/react-router";

export const Route = createFileRoute("/zh/troubleshooting")({
  beforeLoad: () => {
    throw redirect({ href: "/zh#help" });
  },
});
