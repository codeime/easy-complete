import { createFileRoute, redirect } from "@tanstack/react-router";

export const Route = createFileRoute("/zh/install")({
  beforeLoad: () => {
    throw redirect({ href: "/zh#install" });
  },
});
