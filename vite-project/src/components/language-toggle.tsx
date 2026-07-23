
import { Languages } from "lucide-react"
import { useState } from "react"
import { useTranslation } from "react-i18next"

import { Button } from "@/components/ui/button"
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuItem,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { changeApplicationLanguage } from "@/locales/native-locale"

export function LanguageToggle() {
    useTranslation()
    const [pending, setPending] = useState(false)

    const changeLanguage = async (locale: string) => {
        setPending(true)
        try {
            await changeApplicationLanguage(locale)
        } catch (error) {
            console.error(error)
            window.alert('Language change failed. Please retry after the desktop shell is connected.')
        } finally {
            setPending(false)
        }
    }

    return (
        <DropdownMenu>
            <DropdownMenuTrigger asChild>
                <Button variant="outline" size="icon">
                    <Languages className="h-[1.2rem] w-[1.2rem]" />
                    <span className="sr-only">Toggle language</span>
                </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
                <DropdownMenuItem disabled={pending} onClick={() => void changeLanguage("en-US")}>
                    English
                </DropdownMenuItem>
                <DropdownMenuItem disabled={pending} onClick={() => void changeLanguage("zh-CN")}>
                    中文
                </DropdownMenuItem>
            </DropdownMenuContent>
        </DropdownMenu>
    )
}
