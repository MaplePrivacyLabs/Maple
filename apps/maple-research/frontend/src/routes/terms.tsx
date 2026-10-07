import { createFileRoute } from "@tanstack/react-router";
import { Link } from "@tanstack/react-router";
import { TopNav } from "@/components/TopNav";
import { FullPageMain } from "@/components/FullPageMain";
import { MarketingHeader } from "@/components/MarketingHeader";
import { isAndroid } from "@/utils/platform";

export const Route = createFileRoute("/terms")({
  component: Terms
});

function Terms(): JSX.Element {
  return (
    <>
      <TopNav />
      <FullPageMain>
        <MarketingHeader
          title={
            <h2 className="text-6xl font-light mb-0">
              <span className="dark:text-[hsl(var(--blue))] text-[hsl(var(--purple))]">Terms</span>{" "}
              of Use
            </h2>
          }
          subtitle={
            <p className="text-2xl text-[hsl(var(--marketing-text-muted))] max-w-2xl mx-auto">
              Last revised on April 7, 2026
            </p>
          }
        />

        <div className="flex flex-col gap-8 text-foreground pt-8 max-w-4xl mx-auto w-full">
          <article
            className={
              "flex flex-col gap-4 dark:border-white/10 " +
              "border-[hsl(var(--marketing-card-border))] dark:bg-black/75 " +
              "bg-[hsl(var(--marketing-card))]/80 dark:text-white p-8 border rounded-lg " +
              "[&_h2]:text-2xl [&_h2]:font-medium [&_h2]:mt-6 [&_h2]:mb-2 " +
              "[&_p]:leading-relaxed " +
              "[&_ul]:list-disc [&_ul]:pl-6 [&_ul]:space-y-2 [&_li]:leading-relaxed " +
              "[&_a]:text-[hsl(var(--purple))] dark:[&_a]:text-[hsl(var(--blue))] " +
              "[&_a]:underline hover:[&_a]:no-underline " +
              "[&_strong]:font-semibold"
            }
          >
            <p>
              The website located at <a href="https://opensecret.cloud">https://opensecret.cloud</a>
              ,{" "}
              {isAndroid() ? (
                "https://trymaple.ai"
              ) : (
                <a href="https://trymaple.ai">https://trymaple.ai</a>
              )}
              , and the products Maple AI and OpenSecret (together, the "Products") are copyrighted
              works belonging to Maple Privacy Labs Inc. ("Company", "us", "our", and "we"). Certain
              features of the Products and products may be subject to additional guidelines, terms,
              or rules, which will be posted on the Products in connection with such features. All
              such additional terms, guidelines, and rules are incorporated by reference into these
              Terms.
            </p>
            <p>
              <strong>
                These Terms of Use (these "Terms") set forth the legally binding terms and
                conditions that govern your use of the Products. By accessing or using the Products,
                you are accepting these Terms (on behalf of yourself or the entity that you
                represent), and you represent and warrant that you have the right, authority, and
                capacity to enter into these Terms (on behalf of yourself or the entity that you
                represent). You may not access or use the Products or accept the Terms if you are
                not at least 18 years old. If you do not agree with all of the provisions of these
                Terms, do not access and/or use the Products.
              </strong>
            </p>
            <p>
              <strong>
                Please be aware that Section 11.2 contains provisions governing how to resolve
                disputes between you and Company. Among other things, Section 11.2 includes an
                agreement to arbitrate which requires, with limited exceptions, that all disputes
                between you and us shall be resolved by binding and final arbitration. Section 11.2
                also contains a class action and jury trial waiver. Please read Section 11.2
                carefully.
              </strong>
            </p>

            <h2>1. Accounts</h2>
            <p>
              <strong>1.1. Account Creation.</strong> In order to use certain features of the
              Products, you must register for an account ("Account") and provide certain information
              about yourself as prompted by the account registration form. You represent and warrant
              that: (a) all required registration information you submit is truthful and accurate;
              (b) you will maintain the accuracy of such information. You may delete your Account at
              any time, for any reason, by following the instructions on the Products. Company may
              suspend or terminate your Account in accordance with Section 10.
            </p>
            <p>
              <strong>1.2. Account Responsibilities.</strong> You are responsible for maintaining
              the confidentiality of your Account login information and are fully responsible for
              all activities that occur under your Account. You agree to immediately notify Company
              of any unauthorized use, or suspected unauthorized use of your Account or any other
              breach of security. Company cannot and will not be liable for any loss or damage
              arising from your failure to comply with the above requirements.
            </p>

            <h2>2. Access to the Products</h2>
            <p>
              <strong>2.1. License.</strong> Subject to these Terms, Company grants you a
              non-transferable, non-exclusive, revocable, limited license to use and access the
              Products solely for your own personal, noncommercial use, except as expressly
              permitted under Section 5 (Maple AI-Specific Terms).
            </p>
            <p>
              <strong>2.2. Modification.</strong> Company reserves the right, at any time, to
              modify, suspend, or discontinue the Products (in whole or in part) with or without
              notice to you. You agree that Company will not be liable to you or to any third party
              for any modification, suspension, or discontinuation of the Products or any part
              thereof.
            </p>
            <p>
              <strong>2.3. No Support or Maintenance.</strong> You acknowledge and agree that
              Company will have no obligation to provide you with any support or maintenance in
              connection with the Products.
            </p>
            <p>
              <strong>2.4. Ownership.</strong> You acknowledge that all the intellectual property
              rights, including copyrights, patents, trademarks, and trade secrets, in the Products
              and its content are owned by Company or Company's suppliers. Neither these Terms (nor
              your access to the Products) transfers to you or any third party any rights, title or
              interest in or to such intellectual property rights, except for the limited access
              rights expressly set forth in Section 2.1. Company and its suppliers reserve all
              rights not granted in these Terms. There are no implied licenses granted under these
              Terms.
            </p>
            <p>
              <strong>2.5. Feedback.</strong> If you provide Company with any feedback or
              suggestions regarding the Products ("Feedback"), you hereby assign to Company all
              rights in such Feedback and agree that Company shall have the right to use and fully
              exploit such Feedback and related information in any manner it deems appropriate.
              Company will treat any Feedback you provide to Company as non-confidential and
              non-proprietary. You agree that you will not submit to Company any information or
              ideas that you consider to be confidential or proprietary.
            </p>

            <h2>3. Payment Terms</h2>
            <p>
              <strong>3.1. Free and Paid Versions.</strong> Company offers both free and paid
              versions of the Products. Access to the free version of the Products are provided at
              no charge; however, certain features and functionalities may only be available through
              a paid subscription or one-time purchase (collectively, the "Paid Features"). The
              specific details of the Paid Features and applicable fees will be described on the
              Products or as otherwise communicated to you.
            </p>
            <p>
              <strong>3.2. Subscription Plans and Fees.</strong> If you choose to access Paid
              Features, you agree to pay all applicable fees for the selected subscription plan or
              purchase, as described at the time of your transaction. Fees are non-refundable,
              except as required by law or as expressly stated in these Terms. Company reserves the
              right to modify the pricing for any Paid Features at any time. Any price changes will
              only apply to future billing cycles or new subscriptions.
            </p>
            <p>
              <strong>3.3. Billing and Payment Information.</strong> You must provide accurate and
              complete billing information, including a valid payment method (e.g., credit card or
              other accepted payment methods). By submitting payment information, you authorize
              Company to charge the applicable fees to your payment method. If your payment method
              is invalid or expired, Company may suspend or terminate your access to Paid Features
              until payment is successfully processed.
            </p>
            <p>
              <strong>3.4. Automatic Renewal.</strong> For subscription-based Paid Features, your
              subscription will automatically renew at the end of the subscription period unless you
              cancel prior to the renewal date. The renewal will be at the then-current subscription
              rate unless otherwise communicated. You may cancel your subscription at any time by
              following the instructions provided on the Products or by contacting Company at{" "}
              <a href="mailto:support@trymaple.ai">support@trymaple.ai</a>.
            </p>
            <p>
              <strong>3.5. Free Trials and Promotions.</strong> Company may offer free trials or
              promotional offers for certain Paid Features. These offers are subject to these Terms
              and any additional terms provided at the time of the offer. At the end of a free trial
              or promotional period, your access to the Paid Features will automatically convert to
              a paid subscription unless you cancel prior to the end of the trial or promotional
              period.
            </p>
            <p>
              <strong>3.6. Refunds.</strong> Except as required by law or expressly stated in these
              Terms, all payments for Paid Features are final and non-refundable.
            </p>
            <p>
              <strong>3.7. Taxes.</strong> The fees for Paid Features do not include applicable
              taxes. You are responsible for paying any such taxes that may apply, including but not
              limited to sales, use, value-added, or other similar taxes, except for taxes based
              solely on Company's income.
            </p>
            <p>
              <strong>3.8. Changes to Payment Terms.</strong> Company may change its payment terms,
              fees, or billing practices at any time. Any changes will be effective at the start of
              your next billing cycle or upon your next purchase. Your continued use of the Paid
              Features constitutes your acceptance of such changes.
            </p>

            <h2>4. Indemnification</h2>
            <p>
              You agree to indemnify and hold Company (and its officers, employees, and agents)
              harmless, including costs and attorneys' fees, from any claim or demand made by any
              third party due to or arising out of (a) your use of the Products, (b) your violation
              of these Terms or (c) your violation of applicable laws or regulations. Company
              reserves the right, at your expense, to assume the exclusive defense and control of
              any matter for which you are required to indemnify us, and you agree to cooperate with
              our defense of these claims. You agree not to settle any matter without the prior
              written consent of Company. Company will use reasonable efforts to notify you of any
              such claim, action or proceeding upon becoming aware of it.
            </p>

            <h2>5. Maple AI-Specific Terms</h2>
            <p>
              <strong>5.1. Nature of Service; Data Processing.</strong> Maple AI is a
              general-purpose artificial intelligence assistant. You may input any type of data into
              Maple AI, including sensitive personal information, health-related data, financial
              information, or other categories of regulated data. Company will process all user
              inputs as instructed to provide the Service. Company is not a HIPAA-covered entity,
              licensed financial advisor, legal professional, or medical provider. You assume sole
              responsibility for the data you choose to input and should not rely on Maple AI
              outputs as professional advice in any regulated field. AI-generated outputs may be
              inaccurate, incomplete, or outdated, and you should independently verify any
              information before relying on it.
            </p>
            <p>
              <strong>5.2. Memory and Persistent Context.</strong> If you enable memory features,
              Maple AI will retain information from your interactions over time to provide a more
              personalized experience. All memory data is encrypted and stored within your personal
              encrypted vault. You may view, manage, and delete your memory data at any time through
              your account settings. Disabling memory will stop new data from being retained but
              will not automatically delete previously stored memory data; you may separately
              request deletion of stored memory data. Memory features are subject to the data
              practices described in our Privacy Policy.
            </p>
            <p>
              <strong>5.3. Third-Party Integrations.</strong> You may connect Maple AI to
              third-party services, including but not limited to email providers, cloud storage
              services, health data platforms, and productivity tools. By connecting a third-party
              service, you authorize Company to access, retrieve, and transmit data between Maple AI
              and such third-party service on your behalf, in accordance with the permissions you
              grant. Your use of third-party services remains subject to those services' own terms
              of service and privacy policies. Company is not responsible for the acts, omissions,
              or data practices of any third-party service provider. You may revoke Maple AI's
              access to any connected third-party service at any time through your account settings.
            </p>
            <p>
              <strong>5.4. API Access and Commercial Use.</strong> If you access the Products via an
              Application Programming Interface (API) under a paid plan, Company grants you a
              non-exclusive, revocable, non-transferable license to integrate the API into your
              software applications for commercial purposes, provided that: (a) you comply with all
              applicable technical documentation and usage limits; (b) you agree to be bound by
              these Terms and any additional API-specific terms published by Company; (c) you ensure
              that your end users comply with all applicable laws, including data privacy
              regulations; and (d) you do not use the API to build a product or service that
              competes with the Products. If you use the API to process data of third parties,
              including your end users, you represent that you have obtained all necessary consents
              for such processing, you assume full responsibility for compliance with applicable
              privacy laws, and you shall indemnify Company against any claims arising from your
              data processing activities.
            </p>
            <p>
              <strong>5.5. Team Accounts.</strong> If you subscribe to a Team plan, the individual
              who creates the Team account (the "Team Administrator") accepts these Terms on behalf
              of the Team and is responsible for all activity under the Team account. The Team
              Administrator is responsible for ensuring that all Team members comply with these
              Terms. Pooled credits and usage are shared among Team members as described in the
              applicable plan terms.
            </p>

            <h2>6. Use Restrictions</h2>
            <p>
              <strong>6.1.</strong> You may use the Products only in accordance with these Terms and
              applicable laws and regulations. You agree that the Company may investigate and
              prosecute violations of these Use Restrictions to the fullest extent of the law.
              Company reserves the right to suspend or terminate access to the Products if you
              violate these restrictions or if your use of the Products presents a risk to the
              security, integrity, or reputation of the Products or its users. You agree not to:
            </p>
            <ul>
              <li>
                (a) Use the Products in a manner that violates any applicable laws, regulations, or
                third-party rights, including but not limited to privacy, intellectual property, or
                data protection laws.
              </li>
              <li>
                (b) Reverse engineer, disassemble, decompile, or attempt to derive the source code
                or underlying ideas or algorithms of any part of the Products, except to the extent
                such activities are expressly permitted by law.
              </li>
              <li>
                (c) Use the Products to build or train any machine learning or artificial
                intelligence models that compete with the Products, or otherwise use the Products to
                create derivative tools or services that replicate substantial functionality of the
                Products.
              </li>
              <li>
                (d) Use the Products to process or transmit any information or data that: (i)
                violates any applicable laws or regulations, including personal data that is
                unlawfully obtained or shared; (ii) contains malicious code, viruses, or other
                harmful content designed to damage or disrupt the functionality of the Products or
                any third-party systems; or (iii) violates the rights of any individual or entity,
                including by processing sensitive data without proper consent or authorization.
              </li>
              <li>
                (e) Use the Products in any way that infringes, misappropriates, or violates any
                intellectual property rights, or encourage or enable others to do so.
              </li>
              <li>
                (f) Use the Products to engage in, promote, or facilitate illegal or harmful
                activities, including but not limited to: (i) cybersecurity attacks (e.g., phishing,
                denial of service attacks, or distribution of malware); (ii) harassment, abuse, or
                any other harmful conduct toward individuals or groups; (iii) disinformation, fraud,
                or other deceptive practices; (iv) activities that could reasonably be expected to
                cause harm, whether physical, emotional, reputational, or financial, to others.
              </li>
              <li>
                (g) Use the Products in a way that could disable, overburden, damage, or impair the
                functioning of the Products or interfere with the use or enjoyment of the Products
                by others.
              </li>
              <li>
                (h) Attempt to gain unauthorized access to any systems, networks, or data associated
                with the Products, or circumvent any measures implemented to protect the Products or
                enforce limitations on access or use.
              </li>
              <li>
                (i) Use automated tools, scripts, or software (including bots, scrapers, or
                crawlers) to access or interact with the Products in a manner that violates these
                Terms, or attempt to extract or copy data from the Products without authorization.
              </li>
              <li>
                (j) Modify, reproduce, adapt, distribute, display, publish, or sell any part of the
                Products or their associated intellectual property without prior written
                authorization from the Company.
              </li>
              <li>
                (k) Use the Products for high-risk activities, such as operating medical or
                life-support systems, nuclear facilities, or any other applications where the
                failure of the Products could reasonably be expected to result in death, personal
                injury, or catastrophic property damage.
              </li>
              <li>
                (l) Misrepresent your identity, use another person's account without permission, or
                impersonate any individual or entity in connection with your use of the Products.
              </li>
              <li>
                (m) Use the Products to generate content that sexually exploits or endangers minors
                in any way.
              </li>
              <li>
                (n) Use the Products to generate content designed to facilitate violence, terrorism,
                or the development of weapons, including chemical, biological, radiological, or
                nuclear weapons.
              </li>
              <li>
                (o) Use the Products to generate fraudulent content for the purpose of scams,
                phishing, or impersonation of real persons or entities.
              </li>
              <li>
                (p) Use the Products to conduct or facilitate mass surveillance or tracking of
                individuals without their knowledge or consent.
              </li>
              <li>
                (q) Systematically extract, reverse engineer, or attempt to derive model weights,
                training data, algorithms, or other proprietary technical information from the
                Products.
              </li>
              <li>
                (r) Use the Products to make fully automated decisions in high-stakes domains,
                including but not limited to employment, credit, criminal justice, or housing,
                without meaningful human review and oversight.
              </li>
              <li>
                (s) Attempt to circumvent or disable any safety filters, content moderation systems,
                rate limits, or other protective measures implemented by Company.
              </li>
              <li>
                (t) Use the Products to generate spam, bulk unsolicited communications, or other
                content distributed in violation of applicable anti-spam laws.
              </li>
            </ul>

            <h2>7. Third-Party Links; Other Users</h2>
            <p>
              <strong>7.1. Third-Party Links.</strong> The Products may contain links to third-party
              websites and services. Such Third-Party Links are not under the control of Company,
              and Company is not responsible for any Third-Party Links. Company provides access to
              these Third-Party Links only as a convenience to you, and does not review, approve,
              monitor, endorse, warrant, or make any representations with respect to Third-Party
              Links. You use all Third-Party Links at your own risk, and should apply a suitable
              level of caution and discretion in doing so. When you click on any of the Third-Party
              Links, the applicable third party's terms and policies apply, including the third
              party's privacy and data gathering practices. You should make whatever investigation
              you feel necessary or appropriate before proceeding with any transaction in connection
              with such Third-Party Links.
            </p>
            <p>
              <strong>7.2. Privacy of User Data.</strong> We do not sell, rent, or monetize user
              data in any way. Please read our <Link to="/privacy">Privacy Policy</Link> to
              understand how we safeguard your information. By using the Products, you agree to our
              data practices as described in our Privacy Policy, as well as the transfer of your
              encrypted information and metadata to the United States and other countries where we
              have or use facilities, service providers or partners.
            </p>
            <p>
              <strong>7.3. Other Users.</strong> Your interactions with other Product users are
              solely between you and such users. You agree that Company will not be responsible for
              any loss or damage incurred as the result of any such interactions. If there is a
              dispute between you and any Product user, we are under no obligation to become
              involved.
            </p>
            <p>
              <strong>7.4. Release.</strong> You hereby release and forever discharge Company (and
              our officers, employees, agents, successors, and assigns) from, and hereby waive and
              relinquish, each and every past, present and future dispute, claim, controversy,
              demand, right, obligation, liability, action and cause of action of every kind and
              nature (including personal injuries, death, and property damage), that has arisen or
              arises directly or indirectly out of, or that relates directly or indirectly to, the
              Products (including any interactions with, or act or omission of, other Product users
              or any Third-Party Links). If you are a California resident, you hereby waive
              California Civil Code Section 1542 in connection with the foregoing, which states: "A
              general release does not extend to claims which the creditor or releasing party does
              not know or suspect to exist in his or her favor at the time of executing the release,
              which if known by him or her must have materially affected his or her settlement with
              the debtor or released party."
            </p>

            <h2>8. Disclaimers</h2>
            <p>
              <strong>
                The Products are provided on an "as-is" and "as available" basis, and Company (and
                our suppliers) expressly disclaim any and all warranties and conditions of any kind,
                whether express, implied, or statutory, including all warranties or conditions of
                merchantability, fitness for a particular purpose, title, quiet enjoyment, accuracy,
                or non-infringement. We (and our suppliers) make no warranty that the Products will
                meet your requirements, will be available on an uninterrupted, timely, secure, or
                error-free basis, or will be accurate, reliable, free of viruses or other harmful
                code, complete, legal, or safe. If applicable law requires any warranties with
                respect to the Products, all such warranties are limited in duration to 90 days from
                the date of first use.
              </strong>
            </p>
            <p>
              Some jurisdictions do not allow the exclusion of implied warranties, so the above
              exclusion may not apply to you. Some jurisdictions do not allow limitations on how
              long an implied warranty lasts, so the above limitation may not apply to you.
            </p>

            <h2>9. Limitation on Liability</h2>
            <p>
              <strong>
                To the maximum extent permitted by law, in no event shall Company (or our suppliers)
                be liable to you or any third party for any lost profits, lost data, costs of
                procurement of substitute products, or any indirect, consequential, exemplary,
                incidental, special or punitive damages arising from or relating to these Terms or
                your use of, or inability to use, the Products, even if Company has been advised of
                the possibility of such damages. Access to, and use of, the Products is at your own
                discretion and risk, and you will be solely responsible for any damage to your
                device or computer system, or loss of data resulting therefrom.
              </strong>
            </p>
            <p>
              <strong>
                To the maximum extent permitted by law, notwithstanding anything to the contrary
                contained herein, our liability to you for any damages arising from or related to
                these Terms (for any cause whatsoever and regardless of the form of the action),
                will at all times be limited to a maximum of fifty US dollars. The existence of more
                than one claim will not enlarge this limit. You agree that our suppliers will have
                no liability of any kind arising from or relating to these Terms.
              </strong>
            </p>
            <p>
              Some jurisdictions do not allow the limitation or exclusion of liability for
              incidental or consequential damages, so the above limitation or exclusion may not
              apply to you.
            </p>

            <h2>10. Term and Termination</h2>
            <p>
              Subject to this Section, these Terms will remain in full force and effect while you
              use the Products. We may suspend or terminate your rights to use the Products
              (including your Account) at any time for any reason at our sole discretion, including
              for any use of the Products in violation of these Terms. Upon termination of your
              rights under these Terms, your Account and right to access and use the Products will
              terminate immediately. Company will not have any liability whatsoever to you for any
              termination of your rights under these Terms, including for termination of your
              Account. Even after your rights under these Terms are terminated, the following
              provisions of these Terms will remain in effect: Sections 2.2 through 2.5 and Sections
              3 through 11.
            </p>

            <h2>11. General</h2>
            <p>
              <strong>11.1. Changes.</strong> These Terms are subject to occasional revision, and if
              we make any substantial changes, we may notify you by sending you an e-mail to the
              last e-mail address you provided to us (if any), and/or by prominently posting notice
              of the changes on our website. You are responsible for providing us with your most
              current e-mail address. In the event that the last e-mail address that you have
              provided us is not valid, or for any reason is not capable of delivering to you the
              notice described above, our dispatch of the e-mail containing such notice will
              nonetheless constitute effective notice of the changes described in the notice.
              Continued use of our Products following notice of such changes shall indicate your
              acknowledgement of such changes and agreement to be bound by the terms and conditions
              of such changes.
            </p>
            <p>
              <strong>11.2. Dispute Resolution.</strong> Please read the following arbitration
              agreement in this Section (the "Arbitration Agreement") carefully. It requires you to
              arbitrate disputes with Company, its parent companies, subsidiaries, affiliates,
              successors and assigns and all of their respective officers, directors, employees,
              agents, and representatives (collectively, the "Company Parties") and limits the
              manner in which you can seek relief from the Company Parties.
            </p>
            <p>
              <strong>(a) Applicability of Arbitration Agreement.</strong> You agree that any
              dispute between you and any of the Company Parties relating in any way to the
              Products, the services offered on the Products (the "Services") or these Terms will be
              resolved by binding arbitration, rather than in court, except that (1) you and the
              Company Parties may assert individualized claims in small claims court if the claims
              qualify, remain in such court and advance solely on an individual, non-class basis;
              and (2) you or the Company Parties may seek equitable relief in court for infringement
              or other misuse of intellectual property rights (such as trademarks, trade dress,
              domain names, trade secrets, copyrights, and patents). This Arbitration Agreement
              shall survive the expiration or termination of these Terms and shall apply, without
              limitation, to all claims that arose or were asserted before you agreed to these Terms
              (in accordance with the preamble) or any prior version of these Terms. This
              Arbitration Agreement does not preclude you from bringing issues to the attention of
              federal, state or local agencies. Such agencies can, if the law allows, seek relief
              against the Company Parties on your behalf. For purposes of this Arbitration
              Agreement, "Dispute" will also include disputes that arose or involve facts occurring
              before the existence of this or any prior versions of the Agreement as well as claims
              that may arise after the termination of these Terms.
            </p>
            <p>
              <strong>(b) Informal Dispute Resolution.</strong> There might be instances when a
              Dispute arises between you and Company. If that occurs, Company is committed to
              working with you to reach a reasonable resolution. You and Company agree that good
              faith informal efforts to resolve Disputes can result in a prompt, low-cost and
              mutually beneficial outcome. You and Company therefore agree that before either party
              commences arbitration against the other (or initiates an action in small claims court
              if a party so elects), we will personally meet and confer telephonically or via
              videoconference, in a good faith effort to resolve informally any Dispute covered by
              this Arbitration Agreement ("Informal Dispute Resolution Conference"). If you are
              represented by counsel, your counsel may participate in the conference, but you will
              also participate in the conference.
            </p>
            <p>
              The party initiating a Dispute must give notice to the other party in writing of its
              intent to initiate an Informal Dispute Resolution Conference ("Notice"), which shall
              occur within 45 days after the other party receives such Notice, unless an extension
              is mutually agreed upon by the parties. Notice to Company that you intend to initiate
              an Informal Dispute Resolution Conference should be sent by email to:{" "}
              <a href="mailto:support@trymaple.ai">support@trymaple.ai</a>.
            </p>
            <p>
              The Informal Dispute Resolution Conference shall be individualized such that a
              separate conference must be held each time either party initiates a Dispute, even if
              the same law firm or group of law firms represents multiple users in similar cases,
              unless all parties agree; multiple individuals initiating a Dispute cannot participate
              in the same Informal Dispute Resolution Conference unless all parties agree. In the
              time between a party receiving the Notice and the Informal Dispute Resolution
              Conference, nothing in this Arbitration Agreement shall prohibit the parties from
              engaging in informal communications to resolve the initiating party's Dispute.
              Engaging in the Informal Dispute Resolution Conference is a condition precedent and
              requirement that must be fulfilled before commencing arbitration. The statute of
              limitations and any filing fee deadlines shall be tolled while the parties engage in
              the Informal Dispute Resolution Conference process required by this section.
            </p>
            <p>
              <strong>(c) Arbitration Rules and Forum.</strong> These Terms evidence a transaction
              involving interstate commerce; and notwithstanding any other provision herein with
              respect to the applicable substantive law, the Federal Arbitration Act, 9 U.S.C. § 1
              et seq., will govern the interpretation and enforcement of this Arbitration Agreement
              and any arbitration proceedings. If the Informal Dispute Resolution Process described
              above does not resolve satisfactorily within 60 days after receipt of your Notice, you
              and Company agree that either party shall have the right to finally resolve the
              Dispute through binding arbitration. The Federal Arbitration Act governs the
              interpretation and enforcement of this Arbitration Agreement. The arbitration will be
              conducted by JAMS, an established alternative dispute resolution provider. Disputes
              involving claims and counterclaims with an amount in controversy under $250,000, not
              inclusive of attorneys' fees and interest, shall be subject to JAMS' most current
              version of the Streamlined Arbitration Rules and procedures available at{" "}
              <a href="https://www.jamsadr.com/rules-streamlined-arbitration/">
                https://www.jamsadr.com/rules-streamlined-arbitration/
              </a>
              ; all other claims shall be subject to JAMS's most current version of the
              Comprehensive Arbitration Rules and Procedures, available at{" "}
              <a href="https://www.jamsadr.com/rules-comprehensive-arbitration/">
                https://www.jamsadr.com/rules-comprehensive-arbitration/
              </a>
              . JAMS's rules are also available at www.jamsadr.com or by calling JAMS at
              800-352-5267. A party who wishes to initiate arbitration must provide the other party
              with a request for arbitration (the "Request"). The Request must include: (1) the
              name, telephone number, mailing address, e-mail address of the party seeking
              arbitration and the account username (if applicable) as well as the email address
              associated with any applicable account; (2) a statement of the legal claims being
              asserted and the factual bases of those claims; (3) a description of the remedy sought
              and an accurate, good-faith calculation of the amount in controversy in United States
              Dollars; (4) a statement certifying completion of the Informal Dispute Resolution
              process as described above; and (5) evidence that the requesting party has paid any
              necessary filing fees in connection with such arbitration.
            </p>
            <p>
              If the party requesting arbitration is represented by counsel, the Request shall also
              include counsel's name, telephone number, mailing address, and email address. Such
              counsel must also sign the Request. By signing the Request, counsel certifies to the
              best of counsel's knowledge, information, and belief, formed after an inquiry
              reasonable under the circumstances, that: (1) the Request is not being presented for
              any improper purpose, such as to harass, cause unnecessary delay, or needlessly
              increase the cost of dispute resolution; (2) the claims, defenses and other legal
              contentions are warranted by existing law or by a nonfrivolous argument for extending,
              modifying, or reversing existing law or for establishing new law; and (3) the factual
              and damages contentions have evidentiary support or, if specifically so identified,
              will likely have evidentiary support after a reasonable opportunity for further
              investigation or discovery.
            </p>
            <p>
              Unless you and Company otherwise agree, or the Batch Arbitration process discussed in
              Subsection 11.2(h) is triggered, the arbitration will be conducted in the county where
              you reside. Subject to the JAMS Rules, the arbitrator may direct a limited and
              reasonable exchange of information between the parties, consistent with the expedited
              nature of the arbitration. If the JAMS is not available to arbitrate, the parties will
              select an alternative arbitral forum. Your responsibility to pay any JAMS fees and
              costs will be solely as set forth in the applicable JAMS Rules.
            </p>
            <p>
              You and Company agree that all materials and documents exchanged during the
              arbitration proceedings shall be kept confidential and shall not be shared with anyone
              except the parties' attorneys, accountants, or business advisors, and then subject to
              the condition that they agree to keep all materials and documents exchanged during the
              arbitration proceedings confidential.
            </p>
            <p>
              <strong>(d) Authority of Arbitrator.</strong> The arbitrator shall have exclusive
              authority to resolve all disputes subject to arbitration hereunder including, without
              limitation, any dispute related to the interpretation, applicability, enforceability
              or formation of this Arbitration Agreement or any portion of the Arbitration
              Agreement, except for the following: (1) all Disputes arising out of or relating to
              the subsection entitled "Waiver of Class or Other Non-Individualized Relief,"
              including any claim that all or part of the subsection entitled "Waiver of Class or
              Other Non-Individualized Relief" is unenforceable, illegal, void or voidable, or that
              such subsection entitled "Waiver of Class or Other Non-Individualized Relief" has been
              breached, shall be decided by a court of competent jurisdiction and not by an
              arbitrator; (2) except as expressly contemplated in the subsection entitled "Batch
              Arbitration," all Disputes about the payment of arbitration fees shall be decided only
              by a court of competent jurisdiction and not by an arbitrator; (3) all Disputes about
              whether either party has satisfied any condition precedent to arbitration shall be
              decided only by a court of competent jurisdiction and not by an arbitrator; and (4)
              all Disputes about which version of the Arbitration Agreement applies shall be decided
              only by a court of competent jurisdiction and not by an arbitrator. The arbitration
              proceeding will not be consolidated with any other matters or joined with any other
              cases or parties, except as expressly provided in the subsection entitled "Batch
              Arbitration." The arbitrator shall have the authority to grant motions dispositive of
              all or part of any claim or dispute. The arbitrator shall have the authority to award
              monetary damages and to grant any non-monetary remedy or relief available to an
              individual party under applicable law, the arbitral forum's rules, and these Terms
              (including the Arbitration Agreement). The arbitrator shall issue a written award and
              statement of decision describing the essential findings and conclusions on which any
              award (or decision not to render an award) is based, including the calculation of any
              damages awarded. The arbitrator shall follow the applicable law. The award of the
              arbitrator is final and binding upon you and us. Judgment on the arbitration award may
              be entered in any court having jurisdiction.
            </p>
            <p>
              <strong>(e) Waiver of Jury Trial.</strong>{" "}
              <strong>
                Except as specified in Section 11.2(a) you and the Company Parties hereby waive any
                constitutional and statutory rights to sue in court and have a trial in front of a
                judge or a jury.
              </strong>{" "}
              You and the Company Parties are instead electing that all covered claims and disputes
              shall be resolved exclusively by arbitration under this Arbitration Agreement, except
              as specified in Section 11.2(a) above. An arbitrator can award on an individual basis
              the same damages and relief as a court and must follow these Terms as a court would.
              However, there is no judge or jury in arbitration, and court review of an arbitration
              award is subject to very limited review.
            </p>
            <p>
              <strong>(f) Waiver of Class or Other Non-Individualized Relief.</strong>{" "}
              <strong>
                You and Company agree that, except as specified in subsection 11.2(h), each of us
                may bring claims against the other only on an individual basis and not on a class,
                representative, or collective basis, and the parties hereby waive all rights to have
                any dispute be brought, heard, administered, resolved, or arbitrated on a class,
                collective, representative, or mass action basis. Only individual relief is
                available, and disputes of more than one customer or user cannot be arbitrated or
                consolidated with those of any other customer or user.
              </strong>{" "}
              Subject to this Arbitration Agreement, the arbitrator may award declaratory or
              injunctive relief only in favor of the individual party seeking relief and only to the
              extent necessary to provide relief warranted by the party's individual claim. Nothing
              in this paragraph is intended to, nor shall it, affect the terms and conditions under
              the Subsection 11.2(h) entitled "Batch Arbitration." Notwithstanding anything to the
              contrary in this Arbitration Agreement, if a court decides by means of a final
              decision, not subject to any further appeal or recourse, that the limitations of this
              subsection, "Waiver of Class or Other Non-Individualized Relief," are invalid or
              unenforceable as to a particular claim or request for relief (such as a request for
              public injunctive relief), you and Company agree that that particular claim or request
              for relief (and only that particular claim or request for relief) shall be severed
              from the arbitration and may be litigated in the state or federal courts located in
              the State of Delaware. All other Disputes shall be arbitrated or litigated in small
              claims court. This subsection does not prevent you or Company from participating in a
              class-wide settlement of claims.
            </p>
            <p>
              <strong>(g) Attorneys' Fees and Costs.</strong> The parties shall bear their own
              attorneys' fees and costs in arbitration unless the arbitrator finds that either the
              substance of the Dispute or the relief sought in the Request was frivolous or was
              brought for an improper purpose (as measured by the standards set forth in Federal
              Rule of Civil Procedure 11(b)). If you or Company need to invoke the authority of a
              court of competent jurisdiction to compel arbitration, then the party that obtains an
              order compelling arbitration in such action shall have the right to collect from the
              other party its reasonable costs, necessary disbursements, and reasonable attorneys'
              fees incurred in securing an order compelling arbitration. The prevailing party in any
              court action relating to whether either party has satisfied any condition precedent to
              arbitration, including the Informal Dispute Resolution Process, is entitled to recover
              their reasonable costs, necessary disbursements, and reasonable attorneys' fees and
              costs.
            </p>
            <p>
              <strong>(h) Batch Arbitration.</strong> To increase the efficiency of administration
              and resolution of arbitrations, you and Company agree that in the event that there are
              100 or more individual Requests of a substantially similar nature filed against
              Company by or with the assistance of the same law firm, group of law firms, or
              organizations, within a 30 day period (or as soon as possible thereafter), the JAMS
              shall (1) administer the arbitration demands in batches of 100 Requests per batch
              (plus, to the extent there are less than 100 Requests left over after the batching
              described above, a final batch consisting of the remaining Requests); (2) appoint one
              arbitrator for each batch; and (3) provide for the resolution of each batch as a
              single consolidated arbitration with one set of filing and administrative fees due per
              side per batch, one procedural calendar, one hearing (if any) in a place to be
              determined by the arbitrator, and one final award ("Batch Arbitration").
            </p>
            <p>
              All parties agree that Requests are of a "substantially similar nature" if they arise
              out of or relate to the same event or factual scenario and raise the same or similar
              legal issues and seek the same or similar relief. To the extent the parties disagree
              on the application of the Batch Arbitration process, the disagreeing party shall
              advise the JAMS, and the JAMS shall appoint a sole standing arbitrator to determine
              the applicability of the Batch Arbitration process ("Administrative Arbitrator"). In
              an effort to expedite resolution of any such dispute by the Administrative Arbitrator,
              the parties agree the Administrative Arbitrator may set forth such procedures as are
              necessary to resolve any disputes promptly. The Administrative Arbitrator's fees shall
              be paid by Company.
            </p>
            <p>
              You and Company agree to cooperate in good faith with the JAMS to implement the Batch
              Arbitration process including the payment of single filing and administrative fees for
              batches of Requests, as well as any steps to minimize the time and costs of
              arbitration, which may include: (1) the appointment of a discovery special master to
              assist the arbitrator in the resolution of discovery disputes; and (2) the adoption of
              an expedited calendar of the arbitration proceedings.
            </p>
            <p>
              This Batch Arbitration provision shall in no way be interpreted as authorizing a
              class, collective and/or mass arbitration or action of any kind, or arbitration
              involving joint or consolidated claims under any circumstances, except as expressly
              set forth in this provision.
            </p>
            <p>
              <strong>(i) 30-Day Right to Opt Out.</strong> You have the right to opt out of the
              provisions of this Arbitration Agreement by sending a timely written notice of your
              decision to opt out to the following email:{" "}
              <a href="mailto:support@trymaple.ai">support@trymaple.ai</a>, within 30 days after
              first becoming subject to this Arbitration Agreement. Your notice must include your
              name and address and a clear statement that you want to opt out of this Arbitration
              Agreement. If you opt out of this Arbitration Agreement, all other parts of these
              Terms will continue to apply to you. Opting out of this Arbitration Agreement has no
              effect on any other arbitration agreements that you may currently have with us, or may
              enter into in the future with us.
            </p>
            <p>
              <strong>(j) Invalidity, Expiration.</strong> Except as provided in the subsection
              entitled "Waiver of Class or Other Non-Individualized Relief", if any part or parts of
              this Arbitration Agreement are found under the law to be invalid or unenforceable,
              then such specific part or parts shall be of no force and effect and shall be severed
              and the remainder of the Arbitration Agreement shall continue in full force and
              effect. You further agree that any Dispute that you have with Company as detailed in
              this Arbitration Agreement must be initiated via arbitration within the applicable
              statute of limitation for that claim or controversy, or it will be forever time
              barred. Likewise, you agree that all applicable statutes of limitation will apply to
              such arbitration in the same manner as those statutes of limitation would apply in the
              applicable court of competent jurisdiction.
            </p>
            <p>
              <strong>11.3. Export.</strong> The Products may be subject to U.S. export control laws
              and may be subject to export or import regulations in other countries. You agree not
              to export, reexport, or transfer, directly or indirectly, any U.S. technical data
              acquired from Company, or any products utilizing such data, in violation of the United
              States export laws or regulations.
            </p>
            <p>
              <strong>11.4. Electronic Communications.</strong> The communications between you and
              Company use electronic means, whether you use the Products or send us emails, or
              whether Company posts notices on the Products or communicates with you via email. For
              contractual purposes, you (a) consent to receive communications from Company in an
              electronic form; and (b) agree that all terms and conditions, agreements, notices,
              disclosures, and other communications that Company provides to you electronically
              satisfy any legal requirement that such communications would satisfy if they were in
              writing in hard copy. The foregoing does not affect your non-waivable rights.
            </p>
            <p>
              <strong>11.5. Entire Terms.</strong> These Terms constitute the entire agreement
              between you and us regarding the use of the Products. Our failure to exercise or
              enforce any right or provision of these Terms shall not operate as a waiver of such
              right or provision. The section titles in these Terms are for convenience only and
              have no legal or contractual effect. The word "including" means "including without
              limitation". If any provision of these Terms is, for any reason, held to be invalid or
              unenforceable, the other provisions of these Terms will be unimpaired and the invalid
              or unenforceable provision will be deemed modified so that it is valid and enforceable
              to the maximum extent permitted by law. Your relationship to Company is that of an
              independent contractor, and neither party is an agent or partner of the other. These
              Terms, and your rights and obligations herein, may not be assigned, subcontracted,
              delegated, or otherwise transferred by you without Company's prior written consent,
              and any attempted assignment, subcontract, delegation, or transfer in violation of the
              foregoing will be null and void. Company may freely assign these Terms. The terms and
              conditions set forth in these Terms shall be binding upon assignees.
            </p>
            <p>
              <strong>11.6. Contact Information:</strong> If you have concerns about the Products,
              or wish to contact us for any other reasons, please reach out to{" "}
              <a href="mailto:support@trymaple.ai">support@trymaple.ai</a>.
            </p>
          </article>
        </div>
      </FullPageMain>
    </>
  );
}
