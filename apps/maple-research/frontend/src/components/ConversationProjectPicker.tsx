import { Check, Folder, FolderOpen } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { useOpenSecret } from "@mapleai/sdk";
import { Button } from "@/components/ui/button";
import { cn } from "@/utils/utils";
import { listAllConversationProjects } from "@/utils/paginatedLists";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger
} from "@/components/ui/dropdown-menu";

interface ConversationProjectPickerProps {
  selectedProjectId: string | null;
  onSelect: (projectId: string | null) => void | Promise<void>;
  disabled?: boolean;
}

export function ConversationProjectPicker({
  selectedProjectId,
  onSelect,
  disabled = false
}: ConversationProjectPickerProps) {
  const os = useOpenSecret();
  const userId = os.auth.user?.user.id;
  const { data: projects = [] } = useQuery({
    queryKey: ["conversationProjects", userId],
    queryFn: () => listAllConversationProjects(os),
    enabled: !!userId
  });

  const selectedProject = projects.find((project) => project.id === selectedProjectId) ?? null;
  const isProjectSelected = !!selectedProject;

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          pressScale={false}
          className={cn(
            "group h-8 gap-2 px-2 text-muted-foreground",
            isProjectSelected &&
              "bg-[hsl(var(--maple-primary-container))] text-[hsl(var(--maple-primary))] hover:bg-[hsl(var(--maple-primary-container))] hover:text-[hsl(var(--maple-primary))]"
          )}
          disabled={disabled}
          aria-label={selectedProject ? `Project: ${selectedProject.name}` : "No project selected"}
        >
          {isProjectSelected ? (
            <FolderOpen className="h-4 w-4 will-change-transform transition-transform duration-150 ease-out group-active:scale-90 motion-reduce:transition-none" />
          ) : (
            <Folder className="h-4 w-4 will-change-transform transition-transform duration-150 ease-out group-active:scale-90 motion-reduce:transition-none" />
          )}
          {selectedProject ? (
            <span className="hidden max-w-[120px] truncate md:inline">{selectedProject.name}</span>
          ) : null}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start">
        <DropdownMenuItem onClick={() => onSelect(null)}>
          <Check className={`mr-2 h-4 w-4 ${selectedProjectId ? "invisible" : "visible"}`} />
          <FolderOpen className="mr-2 h-4 w-4" />
          No project
        </DropdownMenuItem>
        {projects.length > 0 ? <DropdownMenuSeparator /> : null}
        {projects.map((project) => (
          <DropdownMenuItem key={project.id} onClick={() => onSelect(project.id)}>
            <Check
              className={`mr-2 h-4 w-4 ${selectedProjectId === project.id ? "visible" : "invisible"}`}
            />
            <Folder className="mr-2 h-4 w-4" />
            {project.name}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
