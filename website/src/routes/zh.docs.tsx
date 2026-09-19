import { createFileRoute, redirect } from "@tanstack/react-router";

export const Route = createFileRoute("/zh/docs")({
  beforeLoad: () => {
    throw redirect({ href: "/zh#install" });
  },
});
